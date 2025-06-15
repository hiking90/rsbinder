// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

/*
 * Copyright (C) 2021 The Android Open Source Project
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! Async support for binder operations.
//!
//! This module provides traits and utilities for running binder transactions
//! asynchronously, allowing integration with async runtimes like Tokio.
//! It enables non-blocking binder operations in async contexts.

use std::future::Future;
use std::pin::Pin;

/// Type alias for a pinned, boxed future.
///
/// This shorthand type helps write cleaner async code without littering it
/// with `Pin` and `Send` bounds, commonly used in binder async operations.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Trait for async thread pools that can execute binder transactions.
///
/// `BinderAsyncPool` provides an abstraction over different async runtime
/// implementations (like Tokio) for spawning binder operations on background
/// threads while maintaining async compatibility.
pub trait BinderAsyncPool {
    /// This function should conceptually behave like this:
    ///
    /// ```text
    /// let result = spawn_thread(|| spawn_me()).await;
    /// return after_spawn(result).await;
    /// ```
    ///
    /// If the spawning fails for some reason, the method may also skip the `after_spawn` closure
    /// and immediately return an error.
    ///
    /// The only difference between different implementations should be which
    /// `spawn_thread` method is used. For Tokio, it would be `tokio::task::spawn_blocking`.
    ///
    /// This method has the design it has because the only way to define a trait that
    /// allows the return type of the spawn to be chosen by the caller is to return a
    /// boxed `Future` trait object, and including `after_spawn` in the trait function
    /// allows the caller to avoid double-boxing if they want to do anything to the value
    /// returned from the spawned thread.
    fn spawn<'a, F1, F2, Fut, A, B, E>(
        spawn_me: F1,
        after_spawn: F2,
    ) -> BoxFuture<'a, Result<B, E>>
    where
        F1: FnOnce() -> A,
        F2: FnOnce(A) -> Fut,
        Fut: Future<Output = Result<B, E>>,
        F1: Send + 'static,
        F2: Send + 'a,
        Fut: Send + 'a,
        A: Send + 'static,
        B: Send + 'a,
        E: From<crate::StatusCode>;
}

/// A runtime for executing an async binder server.
pub trait BinderAsyncRuntime {
    /// Block on the provided future, running it to completion and returning its output.
    fn block_on<F: Future>(&self, future: F) -> F::Output;
}
