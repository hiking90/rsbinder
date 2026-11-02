// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The kernel side of a death or freeze notification registration, which the two registries
//! (`death::DeathRegistry`, `freeze::FreezeRegistry`) keep per handle alike: a request written
//! once for the handle's first registration, a clear once for its last, and a request deferred
//! while a clear is in flight (each registry's module doc says why).

/// A command the caller writes while still holding its registry's lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotifyCmd {
    Request,
    Clear,
}

/// What a clear's done (`BR_CLEAR_*_NOTIFICATION_DONE`) did to the handle's slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClearDone {
    /// The slot is gone; nothing is registered for the handle.
    Removed,
    /// Registrations were added during the clear; the caller writes a request.
    Rerequest,
    /// No clear was in flight for the handle.
    Unexpected,
}

/// A slot's registration with the kernel; `S` is what the kernel reported while it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kernel<S> {
    /// The request was written.
    Registered(S),
    /// The clear was written; `rerequest` if registrations were added since.
    PendingClear { rerequest: bool },
}

impl<S: Default> Kernel<S> {
    /// A new slot, whose request the caller writes.
    pub(crate) fn requested() -> Self {
        Kernel::Registered(S::default())
    }

    /// Another registration on the slot: one during a clear asks for a request after it.
    pub(crate) fn added(&mut self) {
        if let Kernel::PendingClear { rerequest } = self {
            *rerequest = true;
        }
    }

    /// The slot's last registration went: the clear to write, unless one is in flight already.
    pub(crate) fn emptied(&mut self) -> Option<NotifyCmd> {
        match self {
            Kernel::Registered(_) => {
                *self = Kernel::PendingClear { rerequest: false };
                Some(NotifyCmd::Clear)
            }
            Kernel::PendingClear { rerequest } => {
                *rerequest = false;
                None
            }
        }
    }

    /// The clear's done, `registered` saying whether the slot has registrations again. On
    /// [`ClearDone::Removed`] the caller drops the slot.
    pub(crate) fn clear_done(&mut self, registered: bool) -> ClearDone {
        match *self {
            Kernel::Registered(_) => ClearDone::Unexpected,
            Kernel::PendingClear { rerequest } if rerequest && registered => {
                *self = Self::requested();
                ClearDone::Rerequest
            }
            Kernel::PendingClear { .. } => ClearDone::Removed,
        }
    }
}

/// What `ProcessState` needs of either registry once a clear's done arrives.
pub(crate) trait Registrations {
    /// Records the clear's done for `handle` ([`Kernel::clear_done`]).
    fn clear_done(&mut self, handle: u32) -> ClearDone;
    /// Forgets `handle` after its request never reached the kernel.
    fn abort_request(&mut self, handle: u32);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_registration_during_a_clear_is_requested_after_it_unless_it_goes_again() {
        let mut kernel = Kernel::<()>::requested();
        assert_eq!(kernel.emptied(), Some(NotifyCmd::Clear));
        assert_eq!(kernel.emptied(), None, "the clear is in flight already");
        kernel.added();
        assert_eq!(kernel.clear_done(true), ClearDone::Rerequest);
        assert_eq!(kernel, Kernel::Registered(()));

        assert_eq!(kernel.emptied(), Some(NotifyCmd::Clear));
        kernel.added();
        assert_eq!(kernel.emptied(), None, "the late registration went too");
        assert_eq!(kernel.clear_done(false), ClearDone::Removed);
    }

    #[test]
    fn a_done_without_a_clear_is_unexpected_and_changes_nothing() {
        let mut kernel = Kernel::Registered(Some(true));
        assert_eq!(kernel.clear_done(true), ClearDone::Unexpected);
        assert_eq!(kernel, Kernel::Registered(Some(true)));
    }

    #[test]
    fn a_rerequest_starts_from_nothing_reported() {
        let mut kernel = Kernel::Registered(Some(true));
        assert_eq!(kernel.emptied(), Some(NotifyCmd::Clear));
        kernel.added();
        assert_eq!(kernel.clear_done(true), ClearDone::Rerequest);
        assert_eq!(kernel, Kernel::Registered(None));
    }
}
