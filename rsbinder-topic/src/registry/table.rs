// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The registry's state without binder (plan 15 §12.4): the shell hands it keys, callers, times
//! and the results of its sends, so the hermetic tests drive it with keys of their own.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use rsbinder::{BinderResult, Caller};

use crate::named::{FIRST_RETRY, LAST_RETRY};

/// Names with a topic, and apart from them names with a watch: the hub's bound on each map.
pub(crate) const MAX_NAMES: usize = 10_000;
/// Watches of one name, the hub's bound on the callbacks of one service name.
pub(crate) const MAX_WATCHERS: usize = 256;
/// Watches owed a notice in all, the most one round of the retry sends (plan 15 §12.9 D13-e).
pub(crate) const MAX_OWED: usize = MAX_WATCHERS;

/// A binder as the table knows it: equality, and the handle its death link is counted by.
pub(crate) trait Key: Clone + PartialEq {
    /// The handle of a proxy; `None` for a binder of this process, which is not linked.
    fn handle(&self) -> Option<u32>;
}

/// The death links the table asks for: one per handle, on its first use and given up on its last.
pub(crate) trait Links<T, W> {
    fn link_topic(&mut self, handle: u32, topic: &T) -> BinderResult<()>;
    fn link_watcher(&mut self, handle: u32, watcher: &W) -> BinderResult<()>;
    fn unlink(&mut self, handle: u32);
}

/// Why the table refused a change.
#[derive(Debug, PartialEq)]
pub(crate) enum Refused {
    /// Another uid's topic holds the name.
    Taken,
    /// As many names as the table takes.
    Limit,
    /// As many watches of the name as the table takes: the shell may judge them again.
    Full,
    /// The death link failed; nothing changed.
    Link(rsbinder::Status),
}

/// A notice to send for a change: the shell judges the caller and sends it.
pub(crate) struct Notice<T, W> {
    pub name: String,
    pub topic: T,
    pub generation: u64,
    pub watcher: W,
    pub caller: Caller,
}

/// What became of one send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sent {
    Delivered,
    /// Failed as a receiver with no room fails it: sent again later.
    Busy,
    /// Failed for good, or to a binder that died.
    Failed,
}

/// A name's topic.
pub(crate) struct Advertised<T> {
    pub key: T,
    pub uid: u32,
    pub pid: i32,
    pub since: Instant,
    /// The generation of the change that gave the name this topic.
    pub generation: u64,
}

/// One name of [`Table::list`].
pub(crate) struct Listed {
    pub name: String,
    pub uid: u32,
    pub pid: i32,
    pub since: Instant,
    pub watchers: usize,
}

struct Watcher<W> {
    key: W,
    caller: Caller,
    owed: Option<Owed>,
}

/// A notice a watch did not get: sent again at `due` with the topic the name has then.
struct Owed {
    generation: u64,
    attempts: u32,
    due: Instant,
}

/// What the table holds and did, for [`RegistryStats`](super::RegistryStats).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Counts {
    pub topics: usize,
    pub watched: usize,
    pub watchers: usize,
    pub owed: usize,
    pub sent: u64,
    pub failed: u64,
    pub retried: u64,
    pub dropped: u64,
}

pub(crate) struct Table<T, W> {
    topics: BTreeMap<String, Advertised<T>>,
    watchers: BTreeMap<String, Vec<Watcher<W>>>,
    /// One counter for every name, so that an entry made again does not count from zero again.
    generation: u64,
    /// Uses of each handle by the two maps.
    links: BTreeMap<u32, usize>,
    owed: usize,
    counts: Counts,
}

/// The wait before the next send of a notice owed after `attempts` sends of it failed.
fn backoff(attempts: u32) -> Duration {
    let doubled = FIRST_RETRY.checked_mul(1 << attempts.min(16));
    doubled.map_or(LAST_RETRY, |wait| wait.min(LAST_RETRY))
}

impl<T: Key, W: Key> Table<T, W> {
    pub(crate) fn new() -> Self {
        Self {
            topics: BTreeMap::new(),
            watchers: BTreeMap::new(),
            generation: 0,
            links: BTreeMap::new(),
            owed: 0,
            counts: Counts::default(),
        }
    }

    /// Give `name` to `topic` for `uid`; the notices of the change, empty for the same binder.
    pub(crate) fn advertise(
        &mut self,
        (uid, pid): (u32, i32),
        name: &str,
        topic: T,
        cross_uid: bool,
        now: Instant,
        links: &mut dyn Links<T, W>,
    ) -> Result<Vec<Notice<T, W>>, Refused> {
        match self.topics.get(name) {
            Some(held) if held.key == topic => return Ok(Vec::new()),
            Some(held) if held.uid != uid && !cross_uid => return Err(Refused::Taken),
            Some(_) => {}
            None if self.topics.len() >= MAX_NAMES => return Err(Refused::Limit),
            None => {}
        }
        self.retain(topic.handle(), |handle| links.link_topic(handle, &topic))
            .map_err(Refused::Link)?;
        self.generation += 1;
        let advertised = Advertised {
            key: topic.clone(),
            uid,
            pid,
            since: now,
            generation: self.generation,
        };
        if let Some(old) = self.topics.insert(name.to_owned(), advertised) {
            self.release(old.key.handle(), links);
        }
        let watchers = self.watchers.get(name).map_or(&[][..], Vec::as_slice);
        let notices = watchers.iter().map(|watcher| Notice {
            name: name.to_owned(),
            topic: topic.clone(),
            generation: self.generation,
            watcher: watcher.key.clone(),
            caller: watcher.caller.clone(),
        });
        Ok(notices.collect())
    }

    /// Take `name` back from `topic` if `uid` advertised it there; whether it did.
    pub(crate) fn withdraw(
        &mut self,
        uid: u32,
        name: &str,
        topic: &T,
        links: &mut dyn Links<T, W>,
    ) -> bool {
        let held = self.topics.get(name);
        if !held.is_some_and(|held| held.key == *topic && held.uid == uid) {
            return false;
        }
        if let Some(old) = self.topics.remove(name) {
            self.generation += 1;
            self.release(old.key.handle(), links);
        }
        self.clear_owed(name);
        true
    }

    /// The topic `name` has and the generation of the change that gave it; with none, now's.
    pub(crate) fn lookup(&self, name: &str) -> (Option<T>, u64) {
        match self.topics.get(name) {
            Some(held) => (Some(held.key.clone()), held.generation),
            None => (None, self.generation),
        }
    }

    /// Watch `name` for `caller`; what [`lookup`](Self::lookup) answers, the reply of the watch.
    pub(crate) fn watch(
        &mut self,
        name: &str,
        watcher: W,
        caller: Caller,
        links: &mut dyn Links<T, W>,
    ) -> Result<(Option<T>, u64), Refused> {
        match self.watchers.get(name) {
            Some(list) if list.iter().any(|held| held.key == watcher) => {
                return Ok(self.lookup(name));
            }
            Some(list) if list.len() >= MAX_WATCHERS => return Err(Refused::Full),
            Some(_) => {}
            None if self.watchers.len() >= MAX_NAMES => return Err(Refused::Limit),
            None => {}
        }
        self.retain(watcher.handle(), |handle| {
            links.link_watcher(handle, &watcher)
        })
        .map_err(Refused::Link)?;
        let entry = Watcher {
            key: watcher,
            caller,
            owed: None,
        };
        self.watchers
            .entry(name.to_owned())
            .or_default()
            .push(entry);
        Ok(self.lookup(name))
    }

    /// The watches of `name` with their callers, for the shell to judge again.
    pub(crate) fn callers_of(&self, name: &str) -> Vec<(W, Caller)> {
        let watchers = self.watchers.get(name).map_or(&[][..], Vec::as_slice);
        let held = watchers.iter().map(|w| (w.key.clone(), w.caller.clone()));
        held.collect()
    }

    /// End the watches of `name` by `watcher`, one or every one of `evicted`.
    pub(crate) fn unwatch(&mut self, name: &str, evicted: &[W], links: &mut dyn Links<T, W>) {
        let Some(list) = self.watchers.get_mut(name) else {
            return;
        };
        let mut gone = Vec::new();
        list.retain_mut(|held| {
            let keep = !evicted.contains(&held.key);
            if !keep {
                gone.push((held.key.handle(), held.owed.is_some()));
            }
            keep
        });
        if list.is_empty() {
            self.watchers.remove(name);
        }
        for (handle, owed) in gone {
            self.owed -= usize::from(owed);
            self.release(handle, links);
        }
    }

    /// The names under `prefix` with a topic, in order.
    pub(crate) fn list(&self, prefix: &str) -> Vec<Listed> {
        let from = self.topics.range::<str, _>((
            std::ops::Bound::Included(prefix),
            std::ops::Bound::Unbounded,
        ));
        let under = from.take_while(|(name, _)| name.starts_with(prefix));
        let listed = under.map(|(name, held)| Listed {
            name: name.clone(),
            uid: held.uid,
            pid: held.pid,
            since: held.since,
            watchers: self.watchers.get(name).map_or(0, Vec::len),
        });
        listed.collect()
    }

    /// The binder of `handle` died: its topics and its watches go, and its link.
    pub(crate) fn forget(&mut self, handle: u32) {
        let dead: Vec<String> = self
            .topics
            .iter()
            .filter(|(_, held)| held.key.handle() == Some(handle))
            .map(|(name, _)| name.clone())
            .collect();
        for name in dead {
            self.topics.remove(&name);
            self.generation += 1;
            self.clear_owed(&name);
        }
        let mut owed = 0;
        self.watchers.retain(|_, list| {
            list.retain(|held| {
                let keep = held.key.handle() != Some(handle);
                owed += usize::from(!keep && held.owed.is_some());
                keep
            });
            !list.is_empty()
        });
        self.owed -= owed;
        self.links.remove(&handle);
    }

    /// Count a send of a change's notice; whether it left a notice owed that was not before.
    pub(crate) fn sent(
        &mut self,
        name: &str,
        watcher: &W,
        generation: u64,
        sent: Sent,
        now: Instant,
    ) -> bool {
        if sent == Sent::Delivered {
            self.counts.sent += 1;
        } else {
            self.counts.failed += 1;
        }
        let (mut owed, mut dropped) = (self.owed, false);
        let Some(held) = find(&mut self.watchers, name, watcher) else {
            return false;
        };
        let added = match (sent, &mut held.owed) {
            (Sent::Delivered, Some(o)) if o.generation <= generation => {
                held.owed = None;
                owed -= 1;
                false
            }
            (Sent::Delivered, _) => false,
            (Sent::Busy, Some(o)) => {
                o.generation = o.generation.max(generation);
                false
            }
            (Sent::Busy, None) if owed >= MAX_OWED => {
                dropped = true;
                false
            }
            (Sent::Busy, None) => {
                held.owed = Some(Owed {
                    generation,
                    attempts: 0,
                    due: now + backoff(0),
                });
                owed += 1;
                true
            }
            (Sent::Failed, Some(_)) => {
                held.owed = None;
                owed -= 1;
                false
            }
            (Sent::Failed, None) => false,
        };
        self.owed = owed;
        self.counts.dropped += u64::from(dropped);
        added
    }

    /// The owed notices due at `now`: name, watcher and the caller to judge.
    pub(crate) fn due(&self, now: Instant) -> Vec<(String, W, Caller)> {
        let mut due = Vec::new();
        for (name, list) in &self.watchers {
            for held in list {
                if held.owed.as_ref().is_some_and(|o| o.due <= now) {
                    due.push((name.clone(), held.key.clone(), held.caller.clone()));
                }
            }
        }
        due
    }

    /// When the next owed notice is due, if any is owed.
    pub(crate) fn next_due(&self) -> Option<Instant> {
        let owed = self
            .watchers
            .values()
            .flatten()
            .filter_map(|w| w.owed.as_ref());
        owed.map(|o| o.due).min()
    }

    /// The topic to send for an owed notice; `None` if none is owed, or no topic, which clears it.
    pub(crate) fn retry_target(&mut self, name: &str, watcher: &W) -> Option<(T, u64)> {
        let topic = self
            .topics
            .get(name)
            .map(|held| (held.key.clone(), held.generation));
        let held = find(&mut self.watchers, name, watcher)?;
        held.owed.as_ref()?;
        if topic.is_none() {
            held.owed = None;
            self.owed -= 1;
        }
        topic
    }

    /// Count a send of an owed notice.
    pub(crate) fn retried(
        &mut self,
        name: &str,
        watcher: &W,
        generation: u64,
        sent: Sent,
        now: Instant,
    ) {
        self.counts.retried += 1;
        if sent != Sent::Delivered {
            self.counts.failed += 1;
        }
        let Some(held) = find(&mut self.watchers, name, watcher) else {
            return;
        };
        let Some(o) = &mut held.owed else {
            return;
        };
        match sent {
            Sent::Delivered if o.generation > generation => {}
            Sent::Delivered | Sent::Failed => {
                held.owed = None;
                self.owed -= 1;
            }
            Sent::Busy => {
                o.attempts += 1;
                o.due = now + backoff(o.attempts);
            }
        }
    }

    /// An owed notice whose caller the policy refuses now: not sent, asked again later.
    pub(crate) fn deferred(&mut self, name: &str, watcher: &W, now: Instant) {
        let held = find(&mut self.watchers, name, watcher);
        if let Some(o) = held.and_then(|held| held.owed.as_mut()) {
            o.attempts += 1;
            o.due = now + backoff(o.attempts);
        }
    }

    #[cfg(test)]
    pub(crate) fn owed(&self) -> usize {
        self.owed
    }

    pub(crate) fn counts(&self) -> Counts {
        Counts {
            topics: self.topics.len(),
            watched: self.watchers.len(),
            watchers: self.watchers.values().map(Vec::len).sum(),
            owed: self.owed,
            ..self.counts
        }
    }

    /// One use more of `handle`, linked on its first: the link runs before any change.
    fn retain(
        &mut self,
        handle: Option<u32>,
        link: impl FnOnce(u32) -> BinderResult<()>,
    ) -> BinderResult<()> {
        let Some(handle) = handle else {
            return Ok(());
        };
        if let Some(uses) = self.links.get_mut(&handle) {
            *uses += 1;
            return Ok(());
        }
        link(handle)?;
        self.links.insert(handle, 1);
        Ok(())
    }

    /// One use less of `handle`, unlinked on its last.
    fn release(&mut self, handle: Option<u32>, links: &mut dyn Links<T, W>) {
        let Some(handle) = handle else {
            return;
        };
        let Some(uses) = self.links.get_mut(&handle) else {
            return;
        };
        *uses -= 1;
        if *uses == 0 {
            self.links.remove(&handle);
            links.unlink(handle);
        }
    }

    /// The name has no topic: no watch of it is owed a notice.
    fn clear_owed(&mut self, name: &str) {
        let Some(list) = self.watchers.get_mut(name) else {
            return;
        };
        for held in list {
            if held.owed.take().is_some() {
                self.owed -= 1;
            }
        }
    }

    /// The death links held, for a test.
    #[cfg(test)]
    pub(crate) fn linked(&self) -> BTreeMap<u32, usize> {
        self.links.clone()
    }

    /// The due of an owed notice, for a test.
    #[cfg(test)]
    pub(crate) fn due_of(&mut self, name: &str, watcher: &W) -> Option<Instant> {
        find(&mut self.watchers, name, watcher)?
            .owed
            .as_ref()
            .map(|o| o.due)
    }
}

fn find<'t, W: PartialEq>(
    watchers: &'t mut BTreeMap<String, Vec<Watcher<W>>>,
    name: &str,
    watcher: &W,
) -> Option<&'t mut Watcher<W>> {
    watchers
        .get_mut(name)?
        .iter_mut()
        .find(|held| held.key == *watcher)
}
