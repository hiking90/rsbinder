// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! What both ends of the subscription call share: the token, the refusals, and how a call that
//! never left this process is told from one that came through binder.
//!
//! A publisher takes a token that lives in its own process only from a call that never left
//! it: [`Subscriber::request`](crate::Subscriber::request) marks the thread with the request it
//! sends ([`Asking`]) and a local `ITopic` runs on that thread with that very request. A token of
//! this process that came through binder is one the caller got from this process, so its death
//! says nothing about the caller, and it is refused. The pid of the call cannot tell the two
//! apart: a handler that subscribes to a topic of its own process still sees its caller's pid.
//! Nor can the token alone: while the thread waits for the reply of a remote `ITopic`, the
//! kernel runs on it a call that remote process makes back into this one, and that call can
//! carry the token it was just sent. Its request is one read from a parcel, never the request
//! the thread is sending, so the address of the request tells the two apart.

use std::cell::Cell;
use std::sync::Weak;

use rsbinder::{BinderResult, ExceptionCode, Interface, Status, StatusCode};

use crate::aidl::ISubscription::ISubscription;
use crate::aidl::SubscribeRequest::SubscribeRequest;
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
    pub(crate) fn detached() -> rsbinder::SIBinder {
        use crate::aidl::ISubscription::BnSubscription;
        let token = Self { local: Weak::new() };
        rsbinder::Interface::as_binder(&BnSubscription::new_binder(token))
    }
}

thread_local! {
    /// The address of the request this thread subscribes with, while it does; 0 for none.
    #[cfg_attr(
        target_os = "android",
        expect(
            clippy::missing_const_for_thread_local,
            reason = "the initializer is `const`; Android's expansion of the macro hides that"
        )
    )]
    static ASKING: Cell<usize> = const { Cell::new(0) };
}

/// Marks this thread as subscribing with `request`, which outlives the mark, until dropped.
/// Not `Send`: the drop restores the mark of the thread that made it.
pub(crate) struct Asking<'a>(
    usize,
    std::marker::PhantomData<(&'a SubscribeRequest, *const ())>,
);

impl<'a> Asking<'a> {
    pub(crate) fn new(request: &'a SubscribeRequest) -> Self {
        Self(ASKING.replace(address(request)), std::marker::PhantomData)
    }
}

impl Drop for Asking<'_> {
    fn drop(&mut self) {
        ASKING.set(self.0);
    }
}

fn address(request: &SubscribeRequest) -> usize {
    std::ptr::from_ref(request) as usize
}

/// Whether `request` is the one of a subscription call this thread is making itself.
pub(crate) fn asked_here(request: &SubscribeRequest) -> bool {
    ASKING.get() == address(request)
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

/// [`status_of`] for a check of the caller's own policy: `EX_ILLEGAL_ARGUMENT` with the check
/// that failed. A status carries a message only with an exception, as AOSP's does.
pub(crate) fn policy_status(error: rsbinder::fmq::Error) -> Status {
    match error {
        rsbinder::fmq::Error::BadValue(_) => {
            Status::from((ExceptionCode::IllegalArgument, error.to_string().as_str()))
        }
        other => status_of(other),
    }
}
