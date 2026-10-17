// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! What both ends of the subscription call share: the token, the refusals, and how a call that
//! never left this process is told from one that came through binder.
//!
//! A publisher takes a token that lives in its own process only from a call that never left
//! it: [`Subscriber::request`](crate::Subscriber::request) marks the thread with its token
//! ([`Asking`]) and a local `ITopic` runs on that thread. A token of this process that came
//! through binder is one the caller got from this process, so its death says nothing about the
//! caller, and it is refused. The pid of the call cannot tell the two apart: a handler that
//! subscribes to a topic of its own process still sees its caller's pid.

use std::cell::RefCell;
use std::sync::Weak;

use rsbinder::{BinderResult, ExceptionCode, Interface, SIBinder, Status, StatusCode};

use crate::aidl::ISubscription::ISubscription;
use crate::wait::{Local, Parts, EVICTED};

/// The binder a subscriber sends as `SubscribeRequest.token`, an `ISubscription`: the publisher
/// links to its death, and tells it through it that the subscription was taken out.
pub(crate) struct Token {
    /// The subscriber's waits; gone with the subscriber, after which a call does nothing.
    pub(crate) local: Weak<Local<Parts>>,
}

impl Interface for Token {}

impl ISubscription for Token {
    fn r#evicted(&self) -> BinderResult<()> {
        if let Some(local) = self.local.upgrade() {
            local.raise(EVICTED);
        }
        Ok(())
    }
}

impl Token {
    /// The token of no subscriber, for tests that make the subscription call themselves.
    #[cfg(test)]
    pub(crate) fn detached() -> SIBinder {
        use crate::aidl::ISubscription::BnSubscription;
        let token = Self { local: Weak::new() };
        rsbinder::Interface::as_binder(&BnSubscription::new_binder(token))
    }
}

thread_local! {
    /// The token of the subscription call this thread is making, while it makes it.
    #[cfg_attr(
        target_os = "android",
        expect(
            clippy::missing_const_for_thread_local,
            reason = "the initializer is `const`; Android's expansion of the macro hides that"
        )
    )]
    static ASKING: RefCell<Option<SIBinder>> = const { RefCell::new(None) };
}

/// Marks this thread as making a subscription call with `token` until dropped.
pub(crate) struct Asking(Option<SIBinder>);

impl Asking {
    pub(crate) fn new(token: &SIBinder) -> Self {
        Self(ASKING.replace(Some(token.clone())))
    }
}

impl Drop for Asking {
    fn drop(&mut self) {
        ASKING.set(self.0.take());
    }
}

/// Whether `token` came with a subscription call this thread is making itself.
pub(crate) fn asked_here(token: &SIBinder) -> bool {
    ASKING.with_borrow(|asking| asking.as_ref() == Some(token))
}

/// A refusal with one of `ITopic`'s service-specific codes.
pub(crate) fn refuse(code: i32, message: &str) -> Status {
    Status::new_service_specific_error(code, Some(message.to_owned()))
}

/// A shared-memory error as a status: `Unsupported` stays apart, the rest is a status code.
pub(crate) fn status_of(error: rsbinder::fmq::Error) -> Status {
    match error {
        rsbinder::fmq::Error::Unsupported => Status::from((
            ExceptionCode::UnsupportedOperation,
            "topics need Linux or Android with F_SEAL_FUTURE_WRITE",
        )),
        other => Status::from(StatusCode::from(other)),
    }
}
