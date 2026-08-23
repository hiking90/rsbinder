// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Access-control policy for the service manager (`rsb_hub`).
//!
//! # Why not SELinux
//!
//! AOSP's `servicemanager` gates every entry point through SELinux
//! (`frameworks/native/cmds/servicemanager/Access.cpp`). SELinux is not
//! available on every Linux distribution and does not exist on macOS, so
//! it cannot be the *basis* of the model here. It does not need to be:
//! AOSP's policy **model** is SELinux-independent —
//!
//! ```text
//! (caller identity, service name) -> {add, find, list}
//! ```
//!
//! — and SELinux is merely one backend for expressing "caller identity".
//! This module keeps the model and swaps the identity source for one that
//! is portable and trustworthy everywhere.
//!
//! # Why uid is the only key
//!
//! | identity | usable for authorization | why |
//! |---|---|---|
//! | uid | yes, authoritative | the binder driver fills `sender_euid` from the sending task's credentials at transaction time — unforgeable, not racy |
//! | pid | **no** | AOSP names the field `debugPid` for a reason: pid reuse races make every pid-derived attribute (`/proc/<pid>/exe`, cgroup, systemd unit) unsound as an authorization key |
//! | gid | indirectly | not delivered by the kernel, but resolving uid → groups through NSS is race-free |
//! | SELinux sid | optionally | only present on an LSM kernel, and only when the target binder opted into `set_requesting_sid` |
//!
//! So: uid (and the user/groups derived from it) is the primary key, with
//! an LSM context available later as an *additional* AND-ed layer rather
//! than a replacement. This is the same conclusion D-Bus reached for the
//! same problem — uid/group policy files as the portable base, LSM as an
//! optional overlay.
//!
//! # Default deny
//!
//! Every permission defaults to deny: a name with no matching rule, a rule
//! with no entry for the permission, and a policy that fails to load all
//! deny. `rsb_hub` refuses to start without a policy rather than falling
//! back to a permissive mode.

mod config;
mod enforcer;
mod model;

pub use config::{load, parse_file, ConfigError, NameResolver, SystemResolver};
pub use enforcer::Enforcer;
pub use model::{NamePattern, Permission, Policy, PolicyError, Rule, Subject, Subjects};
