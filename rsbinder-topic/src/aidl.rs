// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The AIDL of a subscription, as `rsbinder-aidl` generates it from `aidl/rsbinder/topic/`.
//!
//! An application uses these directly only to implement [`ITopic`](ITopic::ITopic) itself, which
//! it does to decide who may subscribe: its `subscribe` checks the caller
//! (`rsbinder::CallingContext`) and hands the request to an [`Acceptor`](crate::Acceptor). Its
//! `.aidl` does not import this package: a method that offers a topic returns `IBinder`, and the
//! subscriber passes that binder to [`Subscriber::request`](crate::Subscriber::request). Copying
//! these files into another crate's AIDL build makes types of the same names that this crate does
//! not take.

mod generated {
    include!(concat!(env!("OUT_DIR"), "/topic.rs"));
}

pub use generated::rsbinder::topic::*;
