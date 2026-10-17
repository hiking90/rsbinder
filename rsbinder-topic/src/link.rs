// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The publisher's list of subscriptions, and the death links both ends hold.

use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use rsbinder::{DeathRecipient, SIBinder, WIBinder};

use crate::layout::{seen_field, ControlView, CLOSED, WAITING};
use crate::pool::Pins;
use crate::wake::{send_wake, Sent};
use crate::SubscriberInfo;

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
    pid: i32,
    dead: Arc<Dead>,
    /// The `state` of the wait last woken by a wake that ends it, so a wait gets one (§5.5).
    woken: AtomicU32,
    /// The publisher's end of the socket pair of a subscription woken through it (§5.5a).
    socket: Option<OwnedFd>,
    /// The subscriber closed its end of the socket.
    hung_up: AtomicBool,
    /// Where the subscription started: no record before it is one the subscriber reads.
    start: AtomicU64,
    _death: Option<DeathLink<Watch>>,
}

/// How a link wants waking after a commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wanted {
    No,
    Futex,
    /// Its socket was sent a byte, or found closed.
    Sent,
}

impl Link {
    /// A subscription through `control` from `uid` and `pid`, woken by `socket` if any, gone with
    /// `token`.
    pub(crate) fn new(
        control: ControlView,
        (uid, pid): (u32, i32),
        socket: Option<OwnedFd>,
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
            pid,
            _death: DeathLink::new(token, watch)?,
            dead,
            // No `state` with `WAITING` raised is zero.
            woken: AtomicU32::new(0),
            socket,
            hung_up: AtomicBool::new(false),
            start: AtomicU64::new(0),
        })
    }

    /// After the commit that made `seq`: mark a wait not woken yet as woken; a socket's byte is
    /// sent here, a futex's by the caller.
    fn wake(&self, seq: u32) -> Wanted {
        let state = self.control.state().load();
        if state & WAITING == 0 || state == self.woken.load(Ordering::Relaxed) {
            return Wanted::No;
        }
        // A wait on this `seq` saw the commit: a wake may land before it sleeps and end nothing.
        if state & !(WAITING | CLOSED) == seen_field(seq) {
            return Wanted::No;
        }
        let Some(socket) = &self.socket else {
            self.woken.store(state, Ordering::Relaxed);
            return Wanted::Futex;
        };
        match send_wake(socket.as_fd()) {
            Sent::Pending => {}
            Sent::Closed => self.hung_up.store(true, Ordering::Relaxed),
            // Not counted as woken: the next commit sends again.
            Sent::Failed => return Wanted::Sent,
        }
        self.woken.store(state, Ordering::Relaxed);
        Wanted::Sent
    }

    /// The subscriber's process died, or it dropped the subscription or its wake socket.
    pub(crate) fn ended(&self) -> bool {
        self.dead.get()
            || self.hung_up.load(Ordering::Relaxed)
            || self.control.state().load() & CLOSED != 0
    }

    pub(crate) fn dead(&self) -> bool {
        self.dead.get()
    }

    /// The subscription as its control page reports it, against the publisher's `write`.
    fn info(&self, write: u64) -> SubscriberInfo {
        SubscriberInfo {
            uid: self.uid,
            pid: self.pid,
            // The subscriber writes `read`: one ahead of `write` is a wrong report, not a lag.
            lag_bytes: write.saturating_sub(self.control.read()),
            lost: self.control.lost(),
            waiting: self.control.state().load() & WAITING != 0,
        }
    }
}

/// One topic's subscriptions, under the writer's lock; removals are returned to drop after it.
#[derive(Default)]
pub(crate) struct Links {
    links: Vec<Arc<Link>>,
    /// Wake bytes sent.
    #[cfg(test)]
    sends: u64,
}

impl Links {
    pub(crate) fn len(&self) -> usize {
        self.links.len()
    }

    pub(crate) fn of_uid(&self, uid: u32) -> usize {
        self.links.iter().filter(|link| link.uid == uid).count()
    }

    /// Each subscription as its control page reports it, against the publisher's `write`.
    pub(crate) fn infos(&self, write: u64) -> Vec<SubscriberInfo> {
        self.links.iter().map(|link| link.info(write)).collect()
    }

    /// Add a subscription that starts at `start`.
    pub(crate) fn push(&mut self, link: Arc<Link>, start: u64) {
        link.start.store(start, Ordering::Relaxed);
        self.links.push(link);
    }

    /// The lowest read position among the subscriptions, each as its control page reports it and
    /// no lower than where it started; `u64::MAX` with none. A slot whose record ends at or below
    /// it is one no subscriber still has to read (`pool.rs`).
    pub(crate) fn lowest_read(&self) -> u64 {
        let read = |link: &Arc<Link>| link.control.read().max(link.start.load(Ordering::Relaxed));
        self.links.iter().map(read).min().unwrap_or(u64::MAX)
    }

    /// After the commit that made `seq`: a byte to each socket waiter, once per wait; whether one
    /// waits on the futex (§5.5, §5.5a).
    pub(crate) fn wake(&mut self, seq: u32) -> bool {
        let mut futex = false;
        // Every link, not up to the first futex one: each remembers the wait it is woken for.
        for link in self.links.iter().filter(|link| !link.ended()) {
            match link.wake(seq) {
                Wanted::No => {}
                Wanted::Futex => futex = true,
                Wanted::Sent => {
                    #[cfg(test)]
                    {
                        self.sends += 1;
                    }
                }
            }
        }
        futex
    }

    /// The wake bytes sent so far.
    #[cfg(all(test, not(loom), any(target_os = "linux", target_os = "android")))]
    pub(crate) fn sends(&self) -> u64 {
        self.sends
    }

    /// Take out the subscriptions that ended.
    pub(crate) fn prune(&mut self) -> Vec<Arc<Link>> {
        let mut gone = Vec::new();
        self.links.retain(|link| {
            let ended = link.ended();
            if ended {
                gone.push(link.clone());
            }
            !ended
        });
        gone
    }

    /// Take out the subscription flagged by `dead`.
    ///
    /// Its pin word goes with it: a slot it pinned is lent again.
    pub(crate) fn remove(&mut self, dead: &Arc<Dead>) -> Option<Arc<Link>> {
        let at = self
            .links
            .iter()
            .position(|link| Arc::ptr_eq(&link.dead, dead))?;
        Some(self.links.swap_remove(at))
    }
}

impl Pins for Links {
    fn pinned(&self, slot: u32) -> bool {
        let names = |link: &Arc<Link>| link.control.pins() > 0 && link.control.pin(0) == slot + 1;
        self.links.iter().any(names)
    }
}
