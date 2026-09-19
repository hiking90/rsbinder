// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::any::Any;
use std::time::Duration;

use super::{TransactionObserver, TxnContext};
use crate::Result;

/// Logs one line per served transaction at `debug` level, target
/// `rsbinder::observe`: descriptor, method (or `code=<n>` when the interface
/// carries no name table), one-way flag, caller uid/pid, handler time and
/// the transport-level result.
///
/// ```text
/// tracedemo.ITraceDemo::add oneway=false uid=1000 pid=4242 elapsed=12.3µs result=Ok(())
/// ```
///
/// The line is formatted only when `log` has `debug` enabled for that
/// target.
#[derive(Debug, Default, Clone, Copy)]
pub struct LogObserver;

impl TransactionObserver for LogObserver {
    fn on_transact(&self, _ctx: &TxnContext<'_>) -> Option<Box<dyn Any + Send>> {
        None
    }

    fn on_reply(
        &self,
        ctx: &TxnContext<'_>,
        _tag: Option<Box<dyn Any + Send>>,
        result: &Result<()>,
        elapsed: Duration,
    ) {
        match ctx.method {
            Some(method) => log::debug!(
                target: "rsbinder::observe",
                "{}::{method} oneway={} uid={} pid={} elapsed={elapsed:?} result={result:?}",
                ctx.descriptor, ctx.is_oneway, ctx.calling_uid, ctx.calling_pid,
            ),
            None => log::debug!(
                target: "rsbinder::observe",
                "{} code={} oneway={} uid={} pid={} elapsed={elapsed:?} result={result:?}",
                ctx.descriptor, ctx.code, ctx.is_oneway, ctx.calling_uid, ctx.calling_pid,
            ),
        }
    }
}
