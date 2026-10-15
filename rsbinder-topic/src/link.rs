// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The publisher's list of subscriptions, and the death links both ends hold.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Weak};

use rsbinder::{DeathRecipient, SIBinder, WIBinder};

use crate::layout::{ControlView, CLOSED, WAITING};

/// A death notification registered on a proxy, removed on drop.
pub(crate) struct DeathLink<R: DeathRecipient + 'static> {
    binder: SIBinder,
    recipient: Arc<R>,
}

impl<R: DeathRecipient + 'static> DeathLink<R> {
    /// Link `recipient` to `binder`'s death; `None` for a binder of this process, which has none.
    pub(crate) fn new(binder: &SIBinder, recipient: R) -> rsbinder::Result<Option<Self>> {
        if binder.as_proxy().is_none() {
            return Ok(None);
        }
        let recipient = Arc::new(recipient);
        binder.link_to_death_arc(&recipient)?;
        Ok(Some(Self {
            binder: binder.clone(),
            recipient,
        }))
    }
}

impl<R: DeathRecipient + 'static> Drop for DeathLink<R> {
    fn drop(&mut self) {
        // After the death it is gone already; nothing else can fail here.
        let _ = self.binder.unlink_to_death_arc(&self.recipient);
    }
}

/// Sets a flag when the binder dies.
#[derive(Default)]
pub(crate) struct Dead(AtomicBool);

impl Dead {
    pub(crate) fn get(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

impl DeathRecipient for Dead {
    fn binder_died(&self, _: &WIBinder) {
        self.0.store(true, Ordering::Release);
    }
}

/// What a dead subscriber's link is taken out of.
pub(crate) trait Forget: Send + Sync {
    /// Remove the link whose flag is `dead`, if the list still holds it.
    fn forget(&self, dead: &Arc<Dead>);
}

/// The recipient on a subscriber's token: flags the link, then takes it out of the list.
pub(crate) struct Watch {
    dead: Arc<Dead>,
    list: Weak<dyn Forget>,
}

impl DeathRecipient for Watch {
    fn binder_died(&self, who: &WIBinder) {
        // First, so that a subscription being added sees it and is refused (plan 15 §5.1a).
        self.dead.binder_died(who);
        if let Some(list) = self.list.upgrade() {
            list.forget(&self.dead);
        }
    }
}

/// One subscription as the publisher holds it.
pub(crate) struct Link {
    control: ControlView,
    uid: u32,
    dead: Arc<Dead>,
    /// The `state` of the wait last woken, so a wait gets one wake (plan 15 §5.5). Under the lock.
    woken: AtomicU32,
    _death: Option<DeathLink<Watch>>,
}

impl Link {
    /// A subscription through `control` from `uid`; `token`'s death takes it out of `list`.
    pub(crate) fn new(
        control: ControlView,
        uid: u32,
        token: &SIBinder,
        list: Weak<dyn Forget>,
    ) -> rsbinder::Result<Self> {
        let dead = Arc::new(Dead::default());
        let watch = Watch {
            dead: dead.clone(),
            list,
        };
        Ok(Self {
            control,
            uid,
            _death: DeathLink::new(token, watch)?,
            dead,
            // No `state` with `WAITING` raised is zero.
            woken: AtomicU32::new(0),
        })
    }

    /// Whether the subscriber sleeps in a wait not woken yet, which counts as woken from here on.
    fn wants_wake(&self) -> bool {
        let state = self.control.state().load();
        if state & WAITING == 0 || state == self.woken.load(Ordering::Relaxed) {
            return false;
        }
        self.woken.store(state, Ordering::Relaxed);
        true
    }

    /// The subscriber's process died, or it dropped the subscription.
    pub(crate) fn ended(&self) -> bool {
        self.dead.get() || self.control.state().load() & CLOSED != 0
    }

    pub(crate) fn dead(&self) -> bool {
        self.dead.get()
    }
}

/// One topic's subscriptions, under the writer's lock; removals are returned to drop after it.
#[derive(Default)]
pub(crate) struct Links(Vec<Arc<Link>>);

impl Links {
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    pub(crate) fn of_uid(&self, uid: u32) -> usize {
        self.0.iter().filter(|link| link.uid == uid).count()
    }

    pub(crate) fn push(&mut self, link: Arc<Link>) {
        self.0.push(link);
    }

    /// Whether a subscriber waits for a wake; each such wait counts once (plan 15 §5.5).
    pub(crate) fn wake_wanted(&self) -> bool {
        // Every link, not up to the first: each one remembers the wait this wake is for.
        self.0
            .iter()
            .filter(|link| !link.ended())
            .fold(false, |wanted, link| link.wants_wake() | wanted)
    }

    /// Take out the subscriptions that ended.
    pub(crate) fn prune(&mut self) -> Vec<Arc<Link>> {
        let mut gone = Vec::new();
        self.0.retain(|link| {
            let ended = link.ended();
            if ended {
                gone.push(link.clone());
            }
            !ended
        });
        gone
    }

    /// Take out the subscription flagged by `dead`.
    pub(crate) fn remove(&mut self, dead: &Arc<Dead>) -> Option<Arc<Link>> {
        let at = self
            .0
            .iter()
            .position(|link| Arc::ptr_eq(&link.dead, dead))?;
        Some(self.0.swap_remove(at))
    }
}
