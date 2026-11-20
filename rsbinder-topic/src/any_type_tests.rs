// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A subscriber of any type ([`AnyType`]) within one process: it takes a topic of each kind and
//! payload, and reads the bytes its publisher encoded.

use rsbinder::{ExceptionCode, StatusCode};

use crate::typed_tests::{sample, Sample};
use crate::{AnyType, Bytes, Payload, Publisher, Start, Subscriber};

fn any(topic: &rsbinder::SIBinder) -> Subscriber<AnyType> {
    Subscriber::<AnyType>::builder()
        .start_at(Start::Next)
        .subscribe(topic)
        .unwrap()
}

#[test]
fn a_subscriber_of_any_type_reads_a_parcelable_topic_as_its_encoded_bytes() {
    let publisher = Publisher::<Sample>::events().build().unwrap();
    let mut subscriber = any(&publisher.as_binder());
    assert_eq!(subscriber.topic_type(), Sample::type_name());
    publisher.publish(&sample(7)).unwrap();
    let AnyType(bytes) = subscriber.try_recv().unwrap();
    assert_eq!(bytes, rsbinder::to_bytes(&sample(7)).unwrap());
    let read: Sample = rsbinder::from_bytes(&bytes).unwrap();
    assert_eq!(read, sample(7));
    assert_eq!(subscriber.id(), Some(publisher.stats().subscribers[0].id));
}

#[test]
fn a_subscriber_of_any_type_reads_bytes_and_lent_values_alike() {
    let publisher = Publisher::<Bytes>::events().lend(64).build().unwrap();
    let mut subscriber = any(&publisher.as_binder());
    assert_eq!(subscriber.topic_type(), "bytes");
    publisher.publish(&Bytes(vec![1, 2, 3])).unwrap();
    let mut loan = publisher.loan_slice(4).unwrap();
    loan.copy_from_slice(&[4, 5, 6, 7]);
    loan.publish().unwrap();
    assert_eq!(subscriber.try_recv(), Ok(AnyType(vec![1, 2, 3])));
    assert_eq!(subscriber.try_recv(), Ok(AnyType(vec![4, 5, 6, 7])));
}

#[test]
fn a_subscriber_of_any_type_reads_a_state_topics_current_value() {
    let publisher = Publisher::<Sample>::state().build(&sample(1)).unwrap();
    let mut subscriber = Subscriber::<AnyType>::builder()
        .subscribe(&publisher.as_binder())
        .unwrap();
    assert_eq!(subscriber.topic_kind(), crate::TopicKind::State);
    let AnyType(bytes) = subscriber.try_recv().unwrap();
    assert_eq!(rsbinder::from_bytes::<Sample>(&bytes).unwrap(), sample(1));
}

#[test]
fn a_subscriber_of_a_type_names_its_own_and_another_name_is_still_refused() {
    let publisher = Publisher::<Bytes>::events().build().unwrap();
    let typed = Subscriber::<Bytes>::builder()
        .subscribe(&publisher.as_binder())
        .unwrap();
    assert_eq!(typed.topic_type(), "bytes");
    let other = Subscriber::<Sample>::builder().subscribe(&publisher.as_binder());
    assert_eq!(
        other.err().map(|status| status.transaction_error()),
        Some(StatusCode::BadType)
    );
}

#[test]
fn no_publisher_takes_the_empty_type_name() {
    let refused = Publisher::<AnyType>::events().build();
    assert_eq!(
        refused.err().map(|status| status.exception_code()),
        Some(ExceptionCode::IllegalArgument)
    );
    let refused = Publisher::<AnyType>::state().build(&AnyType(vec![1]));
    assert_eq!(
        refused.err().map(|status| status.exception_code()),
        Some(ExceptionCode::IllegalArgument)
    );
}
