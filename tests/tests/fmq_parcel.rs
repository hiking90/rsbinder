// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Generic parcelables and the builtin `android.hardware.common` types
//! (plan 12 F0), driven through a `Parcel` without a device or a feature:
//! the generated code for `fmqdemo` (`tests/aidl/fmqdemo/IFmqDemo.aidl`)
//! names `rsbinder::fmq::MQDescriptor<i8, SynchronizedReadWrite>` and
//! `rsbinder::NativeHandle` for imports this crate has no `.aidl` for, and
//! a `Tagged<Tag>` of its own whose parameter is a phantom.

use rsbinder::fmq::{MQDescriptor, SynchronizedReadWrite};
use rsbinder::{Interface, NativeHandle, Parcel, Parcelable};

include!(concat!(env!("OUT_DIR"), "/fmq_demo.rs"));

use fmqdemo::IFmqDemo::{BnFmqDemo, IFmqDemo};
use fmqdemo::QueueBundle::QueueBundle;
use fmqdemo::Tagged::Tagged;

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

/// The phantom parameter is not written: two instantiations produce the same bytes.
#[test]
fn generic_parcelable_round_trips_and_ignores_its_parameter() {
    let value = Tagged::<QueueBundle> {
        id: 7,
        label: "seven".into(),
        ..Default::default()
    };
    // `#[derive(PartialEq)]` bounds the parameter, so compare the fields.
    let back = round_trip(&value);
    assert_eq!((back.id, back.label.as_str()), (7, "seven"));
    let ints = Tagged::<i32> {
        id: 7,
        label: "seven".into(),
        ..Default::default()
    };
    assert_eq!(round_trip(&ints), ints);

    assert_eq!(
        rsbinder::to_bytes(&value).unwrap(),
        rsbinder::to_bytes(&ints).unwrap()
    );
}

/// The builtin `MQDescriptor` and `NativeHandle` resolve to the runtime crate's types.
#[test]
fn builtin_descriptor_round_trips_inside_a_user_parcelable() {
    let bundle = QueueBundle {
        queue: MQDescriptor::<i32, SynchronizedReadWrite> {
            quantum: 4,
            flags: 1,
            ..Default::default()
        },
        overflow: None,
        extra: NativeHandle {
            ints: vec![1, 2, 3],
            ..Default::default()
        },
    };
    let back = round_trip(&bundle);
    assert_eq!(back.queue.quantum, 4);
    assert_eq!(back.queue.flags, 1);
    assert!(back.overflow.is_none());
    assert_eq!(back.extra.ints, vec![1, 2, 3]);
    assert!(back.extra.fds.is_empty());
}

/// A real queue's descriptor crosses the parcel with its fd, then attaches and reads.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn a_queue_descriptor_survives_the_parcel_and_attaches() {
    use rsbinder::fmq::{AttachPolicy, Descriptor, MessageQueue};

    let mut writer = MessageQueue::<u8>::create(32, true).unwrap();
    let sent: MQDescriptor<i8, SynchronizedReadWrite> =
        writer.descriptor().unwrap().try_into().unwrap();
    assert_eq!(sent.handle.fds.len(), 1);

    let received = round_trip(&sent);
    assert_eq!(received.handle.fds.len(), 1);
    assert_eq!(received.grantors.len(), 4);
    assert_eq!(received.quantum, 1);

    let policy = AttachPolicy {
        max_capacity: 32,
        require_seal: true,
        require_event_flag: true,
    };
    let desc = Descriptor::try_from(&received).unwrap();
    let mut reader = MessageQueue::<u8>::attach(&desc, &policy).unwrap();
    assert!(writer.write(b"fmq").unwrap());
    let mut out = [0u8; 3];
    assert!(reader.read(&mut out).unwrap());
    assert_eq!(&out, b"fmq");
}

/// Compile check: the generated trait takes the mapped types in `openQueue` and `closeQueue`.
struct Demo;
impl Interface for Demo {}
impl IFmqDemo for Demo {
    fn r#openQueue(
        &self,
        _capacity: i32,
    ) -> rsbinder::BinderResult<MQDescriptor<i8, SynchronizedReadWrite>> {
        Ok(Default::default())
    }
    fn r#closeQueue(
        &self,
        _desc: &MQDescriptor<i8, SynchronizedReadWrite>,
    ) -> rsbinder::BinderResult<()> {
        Ok(())
    }
    fn r#bundle(&self) -> rsbinder::BinderResult<QueueBundle> {
        Ok(Default::default())
    }
    fn r#tag(
        &self,
        value: &Tagged<QueueBundle>,
        others: &[Tagged<i32>],
    ) -> rsbinder::BinderResult<Tagged<QueueBundle>> {
        Ok(Tagged {
            id: value.id + others.len() as i32,
            label: value.label.clone(),
            ..Default::default()
        })
    }
}

#[test]
fn the_generated_trait_is_implementable() {
    let svc = BnFmqDemo::new_binder(Demo);
    let out = svc
        .r#tag(
            &Tagged {
                id: 1,
                label: "x".into(),
                ..Default::default()
            },
            &[Tagged::default(), Tagged::default()],
        )
        .unwrap();
    assert_eq!(out.id, 3);
}
