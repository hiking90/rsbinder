// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The system bus and the session bus: the names every process of the machine shares, and the
//! names each user has to itself. **Experimental.**
//!
//! A [`Bus`] registers, finds, waits for and watches services by name, as [`hub`] does, and
//! [`session`] gives one whose names belong to the calling uid:
//!
//! ```no_run
//! use std::time::Duration;
//! use rsbinder::bus;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # let player: rsbinder::SIBinder = unimplemented!();
//! let session = bus::session()?;
//! session.add("player", player)?;
//! let found = session.wait_for("player", Duration::from_secs(5))?;
//! # Ok(()) }
//! ```
//!
//! # One transport for the machine
//!
//! Both buses run over kernel binder: a machine with a binder device uses it for every bus, and
//! no process chooses another transport for one of them. [`system`] and [`session`] initialize
//! the process's [`ProcessState`] on the default device when nothing has, and fail with
//! [`StatusCode::NameNotFound`] on a machine with no binder device, for which this module has
//! no transport yet. A device that exists and cannot be opened is that open's error, a
//! permission one for instance: the process takes no part in the bus until the device's mode or
//! group lets it in.
//!
//! # The session bus
//!
//! A session name `player` is the service `user/<uid>/player` of the system's service manager,
//! `<uid>` being the effective uid of the calling process ([`Bus::name`]). What keeps other
//! users out is the service manager's policy: `rsb_hub` takes the rule `user/{uid}/*`, under
//! which a name is added and found only by the uid its digits spell. Without that rule the
//! names are as open as the rules that cover them make them.
//!
//! Every user opens the same binder device, so the policy stops another user from finding a
//! name, not from calling a binder it was handed by other means. A service that must refuse
//! other users checks the caller's uid ([`CallingContext`](crate::CallingContext)) in its
//! methods.
//!
//! A name is at most 127 bytes with its `user/<uid>/` in front.
//!
//! # Threads
//!
//! [`Bus::watch`] and the waits hear a registration through a call into this process, which
//! needs the binder thread pool ([`ProcessState::start_thread_pool`]).

use std::sync::Arc;
use std::time::Duration;

use crate::hub::{self, ServiceManager, ServiceWatch};
use crate::{FromIBinder, ProcessState, Result, SIBinder, Status, StatusCode, Strong};

/// The names every process of the machine shares. See the [module](self) for the transport and
/// its errors.
pub fn system() -> Result<Bus> {
    Bus::over_kernel_binder(String::new())
}

/// The names of the calling process's effective uid: each is the system bus's
/// `user/<uid>/<name>`. See the [module](self) for what keeps other users out.
pub fn session() -> Result<Bus> {
    let uid = rustix::process::geteuid().as_raw();
    Bus::over_kernel_binder(format!("user/{uid}/"))
}

/// A bus: [`system`]'s or [`session`]'s. Cloning it is cheap, and every clone is the same bus.
#[derive(Clone)]
pub struct Bus {
    names: Arc<ServiceManager>,
    prefix: String,
}

impl std::fmt::Debug for Bus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bus")
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl Bus {
    fn over_kernel_binder(prefix: String) -> Result<Bus> {
        if !ProcessState::is_initialized() {
            ProcessState::init_default().inspect_err(|err| {
                if *err == StatusCode::NameNotFound {
                    log::error!(
                        "bus: this machine has no binder device ({} or {}), and the bus has no \
                         other transport yet",
                        crate::DEFAULT_BINDER_PATH,
                        crate::LEGACY_BINDER_PATH
                    );
                }
            })?;
        }
        Ok(Bus {
            names: hub::default()?,
            prefix,
        })
    }

    /// The name the service manager holds for this bus's `name`: `name` itself on the system
    /// bus, `user/<uid>/<name>` on a session bus. It is the name to give anything that takes a
    /// service manager's name and not a bus, such as `rsb_service` or a topic registry.
    pub fn name(&self, name: &str) -> String {
        format!("{}{name}", self.prefix)
    }

    /// Register `binder` under `name` ([`ServiceManager::add_service`]).
    pub fn add(&self, name: &str, binder: impl Into<SIBinder>) -> std::result::Result<(), Status> {
        self.names.add_service(&self.name(name), binder)
    }

    /// The service registered under `name` now; `None` for a name with none and for one the
    /// service manager's policy does not let this process find
    /// ([`ServiceManager::check_service`]).
    pub fn check(&self, name: &str) -> Option<SIBinder> {
        self.names.check_service(&self.name(name))
    }

    /// [`check`](Self::check) as the interface `T` ([`ServiceManager::check_interface`]).
    pub fn check_interface<T: FromIBinder + ?Sized>(&self, name: &str) -> Result<Strong<T>> {
        self.names.check_interface(&self.name(name))
    }

    /// The service under `name`, waiting up to `timeout` for one to be registered:
    /// [`StatusCode::TimedOut`] once it passes ([`ServiceManager::wait_for_service_timeout`]).
    pub fn wait_for(&self, name: &str, timeout: Duration) -> std::result::Result<SIBinder, Status> {
        self.names
            .wait_for_service_timeout(&self.name(name), timeout)
    }

    /// [`wait_for`](Self::wait_for) as the interface `T`
    /// ([`ServiceManager::wait_for_interface_timeout`]).
    pub fn wait_for_interface<T: FromIBinder + ?Sized>(
        &self,
        name: &str,
        timeout: Duration,
    ) -> Result<Strong<T>> {
        self.names
            .wait_for_interface_timeout(&self.name(name), timeout)
    }

    /// Call `on_registered` with the binder each time a service is registered under `name`,
    /// and at once with the one it has, until the returned watch is dropped
    /// ([`ServiceManager::watch`]).
    pub fn watch(
        &self,
        name: &str,
        on_registered: impl Fn(SIBinder) + Send + Sync + 'static,
    ) -> std::result::Result<ServiceWatch, Status> {
        self.names.watch(&self.name(name), on_registered)
    }

    /// The names of this bus that have a service and that this process may find, as
    /// [`add`](Self::add) and [`check`](Self::check) take them: a session bus lists its own
    /// names with no `user/<uid>/` in front, and none of the system bus's
    /// ([`ServiceManager::try_list_services`]).
    pub fn list(&self) -> std::result::Result<Vec<String>, Status> {
        let all = self.names.try_list_services(hub::DUMP_FLAG_PRIORITY_ALL)?;
        Ok(under(&self.prefix, all))
    }
}

/// The names of `all` that start with `prefix`, without it.
fn under(prefix: &str, all: Vec<String>) -> Vec<String> {
    all.into_iter()
        .filter_map(|name| name.strip_prefix(prefix).map(str::to_owned))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::under;

    #[test]
    fn a_session_lists_its_own_names_without_the_prefix() {
        let all = || {
            [
                "manager",
                "user/1000/player",
                "user/10000/player",
                "user/1000/a/b",
            ]
            .map(str::to_owned)
            .to_vec()
        };
        assert_eq!(under("user/1000/", all()), ["player", "a/b"]);
        assert_eq!(under("user/2000/", all()), Vec::<String>::new());
        assert_eq!(under("", all()), all());
    }
}
