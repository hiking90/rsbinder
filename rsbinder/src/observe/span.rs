// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! AIDL transaction spans, named as AOSP names its ATrace sections.
//!
//! AOSP (`Binder.cpp` `BBinder::getTraceName`, `ndk/ibinder.cpp`
//! `getTraceSectionName`) names a section
//! `AIDL::<backend>::<descriptor>::<method>::server|client`, with
//! `unknown_code_<n>` for a code the name table does not cover. rsbinder
//! builds the same string with `rust` as the backend and records it as the
//! `name` field of a `tracing` span called `aidl` (target `rsbinder::aidl`,
//! level `TRACE`): a `tracing` span name must be `&'static str`, and the
//! string is only formatted when a subscriber has enabled the span.

use crate::binder::TransactionCode;
use std::fmt;

#[cfg_attr(not(feature = "tracing"), allow(dead_code))]
pub(crate) struct AidlSpanName<'a> {
    pub(crate) descriptor: &'a str,
    pub(crate) method: Option<&'a str>,
    pub(crate) code: TransactionCode,
    pub(crate) server: bool,
}

impl fmt::Display for AidlSpanName<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AIDL::rust::{}::", self.descriptor)?;
        match self.method {
            Some(method) => f.write_str(method)?,
            None => write!(f, "unknown_code_{}", self.code)?,
        }
        f.write_str(if self.server { "::server" } else { "::client" })
    }
}

/// The client-side span of one AIDL call, opened by proxies generated with
/// `rsbinder_aidl::Builder::trace(true)`. Without the `tracing` feature it
/// is empty and [`in_scope`](Self::in_scope) is a plain call.
///
/// Created where the call is made, so the caller's current span is its
/// parent, and entered around the transaction itself, which for an async
/// proxy runs on another thread; hence a `Send` value rather than a guard.
#[doc(hidden)]
#[must_use]
pub struct __ClientSpan {
    #[cfg(feature = "tracing")]
    span: tracing::Span,
}

impl __ClientSpan {
    #[inline]
    pub fn in_scope<R>(&self, f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "tracing")]
        {
            self.span.in_scope(f)
        }
        #[cfg(not(feature = "tracing"))]
        {
            f()
        }
    }
}

/// Generated-code entry point for the client span; see [`__ClientSpan`].
#[doc(hidden)]
#[inline]
pub fn __trace_client(
    descriptor: &'static str,
    method: &'static str,
    code: TransactionCode,
) -> __ClientSpan {
    #[cfg(feature = "tracing")]
    {
        let name = AidlSpanName {
            descriptor,
            method: Some(method),
            code,
            server: false,
        };
        __ClientSpan {
            span: tracing::trace_span!(target: "rsbinder::aidl", "aidl", name = %name),
        }
    }
    #[cfg(not(feature = "tracing"))]
    {
        let _ = (descriptor, method, code);
        __ClientSpan {}
    }
}

#[cfg(feature = "tracing")]
pub use tracing_observer::TracingObserver;

#[cfg(feature = "tracing")]
mod tracing_observer {
    use super::AidlSpanName;
    use crate::observe::{TransactionObserver, TxnContext};
    use crate::Result;
    use std::any::Any;
    use std::time::Duration;

    /// Opens the server-side span of every incoming transaction, as AOSP's
    /// `BBinder::transact` opens an ATrace section: named
    /// `AIDL::rust::<descriptor>::<method>::server` (see the `name` field),
    /// with `unknown_code_<n>` in place of the method when the interface
    /// carries no name table (`rsbinder_aidl::Builder::trace`) or the code is
    /// a meta transaction.
    ///
    /// The span is a `tracing` span called `aidl`, target `rsbinder::aidl`,
    /// level `TRACE`, entered for the duration of the handler; a span the
    /// handler opens, including the client span of a nested AIDL call, is its
    /// child. It is only opened while this observer is installed with
    /// [`set_observer`](crate::observe::set_observer), which keeps a process
    /// that does not trace at one atomic load per transaction.
    ///
    /// Requires the `tracing` feature.
    #[derive(Debug, Default, Clone, Copy)]
    pub struct TracingObserver;

    impl TransactionObserver for TracingObserver {
        fn on_transact(&self, ctx: &TxnContext<'_>) -> Option<Box<dyn Any + Send>> {
            let name = AidlSpanName {
                descriptor: ctx.descriptor,
                method: ctx.method,
                code: ctx.code,
                server: true,
            };
            let span = tracing::trace_span!(target: "rsbinder::aidl", "aidl", name = %name);
            // `Span::enter` returns a guard that is not `Send`, and the tag
            // must be. The two calls run on the same thread (module rule),
            // so enter here and exit in `on_reply` through the subscriber.
            span.with_subscriber(|(id, dispatch)| dispatch.enter(id))?;
            Some(Box::new(span))
        }

        fn on_reply(
            &self,
            _ctx: &TxnContext<'_>,
            tag: Option<Box<dyn Any + Send>>,
            _result: &Result<()>,
            _elapsed: Duration,
        ) {
            if let Some(span) = tag.and_then(|tag| tag.downcast::<tracing::Span>().ok()) {
                span.with_subscriber(|(id, dispatch)| dispatch.exit(id));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_name_follows_aosp() {
        let name = |method, code, server| {
            AidlSpanName {
                descriptor: "a.b.IFoo",
                method,
                code,
                server,
            }
            .to_string()
        };
        assert_eq!(
            name(Some("bar"), 1, true),
            "AIDL::rust::a.b.IFoo::bar::server"
        );
        assert_eq!(
            name(Some("bar"), 1, false),
            "AIDL::rust::a.b.IFoo::bar::client"
        );
        assert_eq!(
            name(None, 0x5f504e47, true),
            "AIDL::rust::a.b.IFoo::unknown_code_1599098439::server"
        );
    }
}
