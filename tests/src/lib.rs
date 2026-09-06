// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

mod test_accessor_rsb_hub;
mod test_client;
mod test_sm;

/// Single-shot service lookup, the shape these tests want.
///
/// `rsbinder::hub` deliberately makes a caller choose how to wait:
/// `wait_for_*` blocks until the service appears, `check_*` will not start
/// an unregistered lazy service, and `try_get_*` is one `getService` call
/// that answers with whatever is registered now. These tests want the
/// last one — the harness starts the service and waits before the client
/// runs, so a lookup that comes back empty is a failure to report, not a
/// race to sit through.
///
/// Collapsing "service manager unreachable" into "not registered" is the
/// same choice: in this harness either one is equally a bug, and the
/// tests read better asserting on `Option` / `expect`.
pub mod lookup {
    use rsbinder::{FromIBinder, SIBinder, Strong};

    pub fn get_service(name: &str) -> Option<SIBinder> {
        rsbinder::hub::try_get_service(name).ok().flatten()
    }

    pub fn get_interface<T: FromIBinder + ?Sized>(name: &str) -> rsbinder::Result<Strong<T>> {
        rsbinder::hub::try_get_interface(name)?.ok_or(rsbinder::StatusCode::NameNotFound)
    }
}
