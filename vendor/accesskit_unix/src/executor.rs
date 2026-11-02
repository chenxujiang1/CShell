// Copyright 2024 The AccessKit Authors. All rights reserved.
// Licensed under the Apache License, Version 2.0 (found in
// the LICENSE-APACHE file) or the MIT license (found in
// the LICENSE-MIT file), at your option.

// Derived from zbus.
// Copyright 2024 Zeeshan Ali Khan.
// Licensed under the MIT license (found in the LICENSE-MIT file).

#[cfg(not(feature = "tokio"))]
use async_executor::Executor as AsyncExecutor;
use std::future::Future;
#[cfg(feature = "tokio")]
use std::marker::PhantomData;
#[cfg(not(feature = "tokio"))]
use std::sync::Arc;

/// A wrapper around the underlying runtime/executor.
///
/// This is used to run asynchronous tasks internally and allows integration with various runtimes.
///
/// **Note:** You can (and should) completely ignore this type when building with `tokio` feature
/// enabled.
#[cfg(not(feature = "tokio"))]
#[derive(Debug, Clone)]
pub(crate) struct Executor<'a> {
    executor: Arc<AsyncExecutor<'a>>,
}
#[cfg(feature = "tokio")]
#[derive(Debug, Clone)]
pub(crate) struct Executor<'a> {
    phantom: PhantomData<&'a ()>,
}

impl Executor<'_> {
    /// Create a new `Executor`.
    pub(crate) fn new() -> Self {
        #[cfg(not(feature = "tokio"))]
        {
            Self {
                executor: Arc::new(AsyncExecutor::new()),
            }
        }

        #[cfg(feature = "tokio")]
        {
            Self {
                phantom: PhantomData,
            }
        }
    }

    /// Runs the executor until the given future completes.
    ///
    /// With `tokio` feature enabled, it just awaits on the `future`.
    pub(crate) async fn run<T>(&self, future: impl Future<Output = T>) -> T {
        #[cfg(not(feature = "tokio"))]
        {
            self.executor.run(future).await
        }
        #[cfg(feature = "tokio")]
        {
            future.await
        }
    }
}
