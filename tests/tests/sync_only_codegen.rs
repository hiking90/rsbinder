// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 17 K-22: interface code generated with `Builder::set_async_support(false)` builds and
//! runs against rsbinder with its default features, where `async` is on.
//!
//! Everything here is in-process: no binder device, no `ProcessState`.

use rsbinder::bridge::Delegating;
use rsbinder::{BinderResult, Interface, Strong};

include!(concat!(env!("OUT_DIR"), "/sync_only.rs"));

use synconly::ISyncOnly::{BnSyncOnly, ISyncOnly, ISyncOnlyDelegate, HASH, VERSION};

struct Svc;

impl Interface for Svc {}

impl ISyncOnly for Svc {
    fn r#add(&self, a: i32, b: i32) -> BinderResult<i32> {
        Ok(a + b)
    }
    fn r#echo(&self, s: &str) -> BinderResult<String> {
        Ok(s.to_owned())
    }
}

#[test]
fn a_sync_only_interface_is_published_and_called() {
    let svc: Strong<dyn ISyncOnly> = BnSyncOnly::new_binder(Svc);
    assert_eq!(svc.r#add(2, 3).expect("add"), 5);
    assert_eq!(svc.r#echo("hi").expect("echo"), "hi");

    let again: Strong<dyn ISyncOnly> = svc
        .as_binder()
        .into_interface()
        .expect("a local binder casts back to its interface");
    assert_eq!(again.r#add(1, 1).expect("add"), 2);
    assert_eq!(svc.r#getInterfaceVersion().expect("version"), VERSION);
    assert_eq!(svc.r#getInterfaceHash().expect("hash"), HASH);
}

/// A service whose meta methods report a fixed version and hash.
struct Versioned(i32, &'static str);

impl Interface for Versioned {}

impl ISyncOnly for Versioned {
    fn r#add(&self, a: i32, b: i32) -> BinderResult<i32> {
        Ok(a + b)
    }
    fn r#echo(&self, s: &str) -> BinderResult<String> {
        Ok(s.to_owned())
    }
    fn r#getInterfaceVersion(&self) -> BinderResult<i32> {
        Ok(self.0)
    }
    fn r#getInterfaceHash(&self) -> BinderResult<String> {
        Ok(self.1.to_owned())
    }
}

/// Plan 17 D-16, in-process: a published service answers at most this module's `VERSION`, and
/// this module's `HASH` when its own version was above it; an older one is answered as it is.
#[test]
fn the_published_version_is_clamped_to_the_module_version() {
    let newer = BnSyncOnly::new_binder(Versioned(VERSION + 2, "newer"));
    assert_eq!(newer.r#getInterfaceVersion().expect("version"), VERSION);
    assert_eq!(newer.r#getInterfaceHash().expect("hash"), HASH);

    let older = BnSyncOnly::new_binder(Versioned(VERSION - 1, "older"));
    assert_eq!(older.r#getInterfaceVersion().expect("version"), VERSION - 1);
    assert_eq!(older.r#getInterfaceHash().expect("hash"), "older");
}

/// Overrides one method; the other and the meta methods take the forwarding defaults.
struct Doubling;

impl ISyncOnlyDelegate for Doubling {
    fn r#add(&self, upstream: &Strong<dyn ISyncOnly>, a: i32, b: i32) -> BinderResult<i32> {
        upstream.r#add(a, b).map(|sum| sum * 2)
    }
}

/// Plan 17 K-19 in sync-only code: `Delegating` with one hook overridden.
#[test]
fn a_delegate_overrides_one_method_and_forwards_the_rest() {
    let upstream = BnSyncOnly::new_binder(Versioned(VERSION - 1, "older"));
    let gateway = BnSyncOnly::new_binder(Delegating::new(upstream, Doubling));
    assert_eq!(gateway.r#add(2, 3).expect("add"), 10, "the hook runs");
    assert_eq!(
        gateway.r#echo("hi").expect("echo"),
        "hi",
        "the default forwards"
    );
    assert_eq!(
        gateway.r#getInterfaceVersion().expect("version"),
        VERSION - 1,
        "the upstream's version is forwarded"
    );
    assert_eq!(gateway.r#getInterfaceHash().expect("hash"), "older");
}
