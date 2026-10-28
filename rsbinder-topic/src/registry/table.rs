// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The registry's state without binder (plan 15 §12.4): the shell hands it keys, callers, times
//! and the results of its sends, so the hermetic tests drive it with keys of their own.

use std::collections::{BTreeMap, BTreeSet};
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
///
/// Keys with the same handle are equal: a process has one proxy per handle.
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
}

/// A notice a watch did not get: sent again at `due` with the topic the name has then.
struct Owed<W> {
    watcher: W,
    /// The watch's, which never changes: a round judges it without visiting the watches.
    caller: Caller,
    generation: u64,
    attempts: u32,
    due: Instant,
}

/// The names a handle is used under, as a topic and as a watcher.
#[derive(Default)]
struct Uses {
    topics: BTreeSet<String>,
    watches: BTreeSet<String>,
}

#[derive(Clone, Copy)]
enum Use {
    Topic,
    Watch,
}

impl Uses {
    fn names(&mut self, kind: Use) -> &mut BTreeSet<String> {
        match kind {
            Use::Topic => &mut self.topics,
            Use::Watch => &mut self.watches,
        }
    }

    fn is_empty(&self) -> bool {
        self.topics.is_empty() && self.watches.is_empty()
    }
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
    /// The uses of each linked handle by the two maps, none empty: a death visits these alone.
    links: BTreeMap<u32, Uses>,
    /// The watches owed a notice, by name, apart from `watchers` so that a round visits these
    /// alone: each one a watch's, at most [`MAX_OWED`] in all, and no list empty.
    owed: BTreeMap<String, Vec<Owed<W>>>,
    counts: Counts,
}

/// The wait before the next send of an owed notice whose sends again failed `attempts` times.
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
            owed: BTreeMap::new(),
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
        self.retain(topic.handle(), name, Use::Topic, |handle| {
            links.link_topic(handle, &topic)
        })
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
            self.release(old.key.handle(), name, Use::Topic, links);
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
            self.release(old.key.handle(), name, Use::Topic, links);
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
        self.retain(watcher.handle(), name, Use::Watch, |handle| {
            links.link_watcher(handle, &watcher)
        })
        .map_err(Refused::Link)?;
        let entry = Watcher {
            key: watcher,
            caller,
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

    /// End the watches of `name` whose watcher is one of `evicted`.
    pub(crate) fn unwatch(&mut self, name: &str, evicted: &[W], links: &mut dyn Links<T, W>) {
        let Some(list) = self.watchers.get_mut(name) else {
            return;
        };
        let mut gone = Vec::new();
        list.retain(|held| {
            let keep = !evicted.contains(&held.key);
            if !keep {
                gone.push(held.key.handle());
            }
            keep
        });
        if list.is_empty() {
            self.watchers.remove(name);
        }
        retain_owed(&mut self.owed, name, |o| !evicted.contains(&o.watcher));
        for handle in gone {
            self.release(handle, name, Use::Watch, links);
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
        let Some(uses) = self.links.remove(&handle) else {
            return;
        };
        for name in uses.topics {
            self.topics.remove(&name);
            self.generation += 1;
            self.clear_owed(&name);
        }
        let other = |key: &W| key.handle() != Some(handle);
        for name in uses.watches {
            if let Some(list) = self.watchers.get_mut(&name) {
                list.retain(|held| other(&held.key));
                if list.is_empty() {
                    self.watchers.remove(&name);
                }
            }
            retain_owed(&mut self.owed, &name, |o| other(&o.watcher));
        }
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
        let Some(held) = find(&self.watchers, name, watcher) else {
            return false;
        };
        let full = self.owed() >= MAX_OWED;
        let other = |o: &Owed<W>| o.watcher != *watcher;
        match (sent, owed_mut(&mut self.owed, name, watcher)) {
            (Sent::Delivered, Some(o)) if o.generation <= generation => {
                retain_owed(&mut self.owed, name, other);
                false
            }
            (Sent::Delivered, _) => false,
            (Sent::Busy, Some(o)) => {
                o.generation = o.generation.max(generation);
                false
            }
            (Sent::Busy, None) if full => {
                self.counts.dropped += 1;
                false
            }
            (Sent::Busy, None) => {
                let owed = Owed {
                    watcher: watcher.clone(),
                    caller: held.caller.clone(),
                    generation,
                    attempts: 0,
                    due: now + backoff(0),
                };
                self.owed.entry(name.to_owned()).or_default().push(owed);
                true
            }
            (Sent::Failed, Some(_)) => {
                retain_owed(&mut self.owed, name, other);
                false
            }
            (Sent::Failed, None) => false,
        }
    }

    /// The owed notices due at `now`: name, watcher, the caller to judge and the owed generation.
    pub(crate) fn due(&self, now: Instant) -> Vec<(String, W, Caller, u64)> {
        let owed = self
            .owed
            .iter()
            .flat_map(|(name, list)| list.iter().map(move |o| (name, o)));
        let due = owed.filter(|(_, o)| o.due <= now);
        let due = due.map(|(name, o)| {
            let caller = o.caller.clone();
            (name.clone(), o.watcher.clone(), caller, o.generation)
        });
        due.collect()
    }

    /// When the next owed notice is due, if any is owed.
    pub(crate) fn next_due(&self) -> Option<Instant> {
        self.owed.values().flatten().map(|o| o.due).min()
    }

    /// The topic to send for an owed notice; `None` if none is owed, or no topic, which clears it.
    pub(crate) fn retry_target(&mut self, name: &str, watcher: &W) -> Option<(T, u64)> {
        let topic = self
            .topics
            .get(name)
            .map(|held| (held.key.clone(), held.generation));
        owed_mut(&mut self.owed, name, watcher)?;
        if topic.is_none() {
            retain_owed(&mut self.owed, name, |o| o.watcher != *watcher);
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
        let Some(o) = owed_mut(&mut self.owed, name, watcher) else {
            return;
        };
        match sent {
            Sent::Delivered if o.generation > generation => {}
            Sent::Delivered | Sent::Failed => {
                retain_owed(&mut self.owed, name, |o| o.watcher != *watcher);
            }
            Sent::Busy => {
                o.attempts += 1;
                o.due = now + backoff(o.attempts);
            }
        }
    }

    /// An owed notice of `generation` the policy refuses now: no longer owed (plan 15 D13-a).
    pub(crate) fn refused(&mut self, name: &str, watcher: &W, generation: u64) {
        let owed = owed_mut(&mut self.owed, name, watcher);
        if owed.is_some_and(|o| o.generation == generation) {
            retain_owed(&mut self.owed, name, |o| o.watcher != *watcher);
        }
    }

    pub(crate) fn owed(&self) -> usize {
        self.owed.values().map(Vec::len).sum()
    }

    pub(crate) fn counts(&self) -> Counts {
        Counts {
            topics: self.topics.len(),
            watched: self.watchers.len(),
            watchers: self.watchers.values().map(Vec::len).sum(),
            owed: self.owed(),
            ..self.counts
        }
    }

    /// One use more of `handle`, under `name`, linked on its first: the link runs before any change.
    fn retain(
        &mut self,
        handle: Option<u32>,
        name: &str,
        kind: Use,
        link: impl FnOnce(u32) -> BinderResult<()>,
    ) -> BinderResult<()> {
        let Some(handle) = handle else {
            return Ok(());
        };
        if let Some(uses) = self.links.get_mut(&handle) {
            let new = uses.names(kind).insert(name.to_owned());
            debug_assert!(new, "{name:?}: two keys of handle {handle}, against `Key`");
            return Ok(());
        }
        link(handle)?;
        let mut uses = Uses::default();
        uses.names(kind).insert(name.to_owned());
        self.links.insert(handle, uses);
        Ok(())
    }

    /// One use less of `handle`, under `name`, unlinked on its last.
    fn release(&mut self, handle: Option<u32>, name: &str, kind: Use, links: &mut dyn Links<T, W>) {
        let Some(handle) = handle else {
            return;
        };
        let Some(uses) = self.links.get_mut(&handle) else {
            return;
        };
        uses.names(kind).remove(name);
        if uses.is_empty() {
            self.links.remove(&handle);
            links.unlink(handle);
        }
    }

    /// The name has no topic: no watch of it is owed a notice.
    fn clear_owed(&mut self, name: &str) {
        self.owed.remove(name);
    }

    /// The death links held, for a test.
    #[cfg(test)]
    pub(crate) fn linked(&self) -> BTreeMap<u32, usize> {
        let uses = self.links.iter();
        let counted = uses.map(|(handle, uses)| (*handle, uses.topics.len() + uses.watches.len()));
        counted.collect()
    }

    /// The due of an owed notice, for a test.
    #[cfg(test)]
    pub(crate) fn due_of(&self, name: &str, watcher: &W) -> Option<Instant> {
        let list = self.owed.get(name)?;
        list.iter().find(|o| o.watcher == *watcher).map(|o| o.due)
    }

    /// Panic on the first broken invariant of the maps (plan 15-pubsub-registry-index §6.2).
    #[cfg(test)]
    pub(crate) fn check(&self) {
        for (name, list) in &self.watchers {
            assert!(!list.is_empty(), "{name:?}: an empty watch list");
            for (i, held) in list.iter().enumerate() {
                let again = list[..i].iter().any(|other| other.key == held.key);
                assert!(!again, "{name:?}: a watcher watches twice");
            }
        }
        for (name, list) in &self.owed {
            assert!(!list.is_empty(), "{name:?}: an empty owed list");
            for (i, o) in list.iter().enumerate() {
                let watched = find(&self.watchers, name, &o.watcher).is_some();
                assert!(watched, "{name:?}: owed to no watch");
                let again = list[..i].iter().any(|other| other.watcher == o.watcher);
                assert!(!again, "{name:?}: a watch owed twice");
            }
        }
        assert!(self.owed() <= MAX_OWED, "{} owed", self.owed());
        type Names<'a> = (BTreeSet<&'a str>, BTreeSet<&'a str>);
        let mut uses: BTreeMap<u32, Names<'_>> = BTreeMap::new();
        for (name, held) in &self.topics {
            if let Some(handle) = held.key.handle() {
                uses.entry(handle).or_default().0.insert(name);
            }
        }
        for (name, list) in &self.watchers {
            for handle in list.iter().filter_map(|held| held.key.handle()) {
                uses.entry(handle).or_default().1.insert(name);
            }
        }
        fn names(set: &BTreeSet<String>) -> BTreeSet<&str> {
            set.iter().map(String::as_str).collect()
        }
        let indexed: BTreeMap<u32, Names<'_>> = self
            .links
            .iter()
            .map(|(handle, held)| (*handle, (names(&held.topics), names(&held.watches))))
            .collect();
        assert_eq!(indexed, uses, "the names of each handle");
    }
}

fn find<'t, W: PartialEq>(
    watchers: &'t BTreeMap<String, Vec<Watcher<W>>>,
    name: &str,
    watcher: &W,
) -> Option<&'t Watcher<W>> {
    watchers.get(name)?.iter().find(|held| held.key == *watcher)
}

fn owed_mut<'t, W: PartialEq>(
    owed: &'t mut BTreeMap<String, Vec<Owed<W>>>,
    name: &str,
    watcher: &W,
) -> Option<&'t mut Owed<W>> {
    owed.get_mut(name)?
        .iter_mut()
        .find(|o| o.watcher == *watcher)
}

/// Keep the notices owed for `name` that `keep` takes; a list emptied leaves the map.
fn retain_owed<W>(
    owed: &mut BTreeMap<String, Vec<Owed<W>>>,
    name: &str,
    keep: impl FnMut(&Owed<W>) -> bool,
) {
    let Some(list) = owed.get_mut(name) else {
        return;
    };
    list.retain(keep);
    if list.is_empty() {
        owed.remove(name);
    }
}
