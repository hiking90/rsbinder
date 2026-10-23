// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Whether a subscriber vouches for its publisher (plan 15 §6.3).

/// Whether a [`Subscriber`](crate::Subscriber) vouches for its topic's publisher: [`Untrusted`],
/// what [`Subscriber::request`](crate::Subscriber::request) makes, or [`Trusted`], what
/// [`trust_publisher`](crate::Subscriber::trust_publisher) makes of one.
///
/// It decides how a lent value is read. A [`Sample`](crate::Sample) of an untrusted subscriber is
/// copied out with atomic loads, or read in place through `unsafe`
/// [`as_slice`](crate::Sample::as_slice); one of a trusted subscriber derefs to `[u8]` and is
/// copied with a plain memory copy. Sealed: these two are all there is.
pub trait Trust: sealed::Sealed + 'static {
    /// Whether this kind of subscriber vouches for the publisher.
    #[doc(hidden)]
    const TRUSTED: bool;
}

/// A subscriber that does not vouch for its publisher, the kind
/// [`Subscriber::request`](crate::Subscriber::request) makes.
///
/// A publisher that breaks the pin protocol and writes a slot while it is read makes this
/// subscriber's copy wrong, and nothing worse.
#[derive(Debug)]
pub enum Untrusted {}

/// A subscriber that vouches for its publisher, made by
/// [`trust_publisher`](crate::Subscriber::trust_publisher).
///
/// Its samples deref to `[u8]`, and every copy of a lent value, a sample's or `recv`'s, is a
/// plain memory copy into a buffer it does not zero first.
#[derive(Debug)]
pub enum Trusted {}

impl Trust for Untrusted {
    const TRUSTED: bool = false;
}

impl Trust for Trusted {
    const TRUSTED: bool = true;
}

mod sealed {
    pub trait Sealed {}

    impl Sealed for super::Untrusted {}
    impl Sealed for super::Trusted {}
}
