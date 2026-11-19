// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The `tracing` events of a subscription's life (feature `tracing`): one per subscription
//! taken, refused, taken out or ended, per topic end, and per connection a subscriber makes or
//! loses. None per record. Each is emitted with the topic's lock let go, under the target
//! `rsbinder_topic`.

/// A `tracing` event of `level` under this crate's target; nothing without the feature, whose
/// arguments are then not compiled either.
macro_rules! event {
    ($level:ident, $($arg:tt)+) => {{
        #[cfg(feature = "tracing")]
        {
            tracing::$level!(target: "rsbinder_topic", $($arg)+);
        }
    }};
}

pub(crate) use event;
