// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 10-9 AC-9.7, runtime half: a service generated with
//! `Builder::trace(true)` names its transactions, and one generated without
//! it does not. Compiling this file is itself part of the check: the name
//! table goes through every `declare_binder_interface!` arm the generator
//! uses, with and without `stability:`, and through the `async` variant this
//! crate's build enables.

use rsbinder::{Remotable, FIRST_CALL_TRANSACTION};

include!(concat!(env!("OUT_DIR"), "/trace_demo.rs"));
include!(concat!(env!("OUT_DIR"), "/async_rt.rs"));

use asyncrt::IAsyncRt::BnAsyncRt;
use tracedemo::ITraceDemo::BnTraceDemo;
use tracedemo::ITraceVintf::BnTraceVintf;

#[test]
fn traced_service_names_its_methods() {
    use tracedemo::ITraceDemo::transactions;
    assert_eq!(
        BnTraceDemo::transaction_name(transactions::r#ping),
        Some("ping")
    );
    assert_eq!(
        BnTraceDemo::transaction_name(transactions::r#add),
        Some("add")
    );
    assert_eq!(
        BnTraceDemo::transaction_name(transactions::r#notify),
        Some("notify")
    );
}

#[test]
fn traced_service_names_the_meta_methods() {
    use tracedemo::ITraceDemo::transactions;
    assert_eq!(
        BnTraceDemo::transaction_name(transactions::r#getInterfaceVersion),
        Some("getInterfaceVersion")
    );
    assert_eq!(
        BnTraceDemo::transaction_name(transactions::r#getInterfaceHash),
        Some("getInterfaceHash")
    );
}

#[test]
fn codes_outside_the_table_have_no_name() {
    assert_eq!(BnTraceDemo::transaction_name(0), None);
    assert_eq!(
        BnTraceDemo::transaction_name(FIRST_CALL_TRANSACTION + 4),
        None
    );
    assert_eq!(
        BnTraceDemo::transaction_name(rsbinder::PING_TRANSACTION),
        None
    );
}

#[test]
fn an_unused_explicit_id_has_no_name() {
    use tracedemo::ITraceVintf::transactions;
    assert_eq!(
        BnTraceVintf::transaction_name(transactions::r#one),
        Some("one")
    );
    assert_eq!(
        BnTraceVintf::transaction_name(FIRST_CALL_TRANSACTION + 1),
        None
    );
    assert_eq!(
        BnTraceVintf::transaction_name(transactions::r#two),
        Some("two")
    );
}

#[test]
fn service_generated_without_trace_names_nothing() {
    assert_eq!(BnAsyncRt::transaction_name(FIRST_CALL_TRANSACTION), None);
}
