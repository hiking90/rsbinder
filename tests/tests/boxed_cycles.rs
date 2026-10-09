// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A reference cycle between parcelables is cut by its one boxed field; every other field on
//! the cycle stays inline. AOSP `CheckNoRecursiveDefinition` (`parser.cpp:144-195`) accepts
//! these shapes, and the generated types must be finite, or this file does not compile.

use rsbinder::{Parcel, Parcelable};

include!(concat!(env!("OUT_DIR"), "/boxed_cycles.rs"));

use boxedcycles::BoxedCycles::{
    Back::Back, Front::Front, HeapLeaf::HeapLeaf, HeapUnion::HeapUnion, Inner::Inner, Leaf::Leaf,
    Node::Node, Outer::Outer, Tree::Tree,
};

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

/// The field types are the assertion: each binding fails to compile if the shape changes.
#[test]
fn heap_nullable_off_a_cycle_is_boxed_as_aosp_renders_it() {
    let leaf: Option<Box<Leaf>> = Some(Box::new(Leaf { value: 7 }));
    let back = round_trip(&HeapLeaf { leaf });
    assert_eq!(back.leaf.map(|l| l.value), Some(7));

    let back = round_trip(&HeapUnion::Leaf(Some(Box::new(Leaf { value: 8 }))));
    match back {
        HeapUnion::Leaf(Some(leaf)) => assert_eq!(leaf.value, 8),
        other => panic!("union came back as {other:?}"),
    }

    let front: Option<Front> = Some(Front::default());
    let back_edge: Option<Box<Back>> = Some(Box::new(Back { front }));
    let back = round_trip(&Front { back: back_edge });
    assert!(back
        .back
        .expect("back")
        .front
        .expect("front")
        .back
        .is_none());
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
