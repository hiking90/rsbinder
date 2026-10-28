// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The client side of a registry of topic names (plan 15 §12.5).

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rsbinder::{hub, BinderResult, Interface, SIBinder, StatusCode, Strong};

use crate::aidl::ITopic::ITopic;
use crate::aidl::ITopicRegistry::{ITopicRegistry, SERVICE_NAME};
use crate::aidl::ITopicWatcher::{BnTopicWatcher, ITopicWatcher};
use crate::aidl::TopicInfo::TopicInfo;
use crate::aidl::TopicRef::TopicRef;
use crate::resolve::{Advertisement, Resolver, Watch};

/// A registry of topic names as a [`Resolver`]: names apart from service names, under the policy
/// of the registry's host.
///
/// `advertise` gives the name to the topic until the [`Advertisement`] is dropped, which takes it
/// back with a oneway call: a [`Publisher`](crate::Publisher) drops it as it closes. `watch`
/// answers the topic the name has in its reply, then hears of each one it is given; its lookup
/// is the registry's `lookup`. Every topic and every answer carries the registry's generation,
/// and the watch passes on none older than one it has seen: a notice still queued for this
/// process, older than an answer the watch already used, does not take a subscriber back to it.
/// The calls of one watch run one at a time, in the order of their generations. A call may drop
/// its own [`Watch`]; dropping a watch does not wait for a call running on another thread, and
/// none begins after it.
///
/// A notice is a call into this process: it needs the binder thread pool
/// (`rsbinder::ProcessState::start_thread_pool`), without which a watch hears only its reply.
///
/// A host that ends takes its names and watches with it. Every call of this resolver then fails
/// with `DeadObject`, publishers and subscribers already connected go on, and a publisher does
/// not advertise its name to the next host: make the publisher and the name subscriber again
/// with a resolver of the new host (plan 15 D8). A refusal of a call is `DeadObject` only when
/// the host died: a binder of the call that is dead is `ILLEGAL_STATE`.
#[derive(Clone)]
pub struct RegistryResolver {
    registry: Strong<dyn ITopicRegistry>,
}

impl RegistryResolver {
    /// The registry the service manager has under [`SERVICE_NAME`]: `NameNotFound` when it has
    /// none, or its policy does not let this process find it.
    ///
    /// [`SERVICE_NAME`]: crate::aidl::ITopicRegistry::SERVICE_NAME
    pub fn find() -> BinderResult<Self> {
        let binder = hub::check_service(SERVICE_NAME).ok_or(StatusCode::NameNotFound)?;
        Self::new(binder)
    }

    /// The registry `binder` is, found by other means than [`find`](Self::find). A registry of
    /// this process is `BadValue`: its calls would come from no transaction, which it refuses.
    /// Another interface's binder is `BadType`.
    pub fn new(binder: SIBinder) -> BinderResult<Self> {
        if binder.as_proxy().is_none() {
            return Err(StatusCode::BadValue.into());
        }
        Ok(Self {
            registry: binder.into_interface_checked()?,
        })
    }

    /// The names under `prefix` with a topic that this process may subscribe to, sorted.
    pub fn list(&self, prefix: &str) -> BinderResult<Vec<TopicInfo>> {
        self.registry.r#list(prefix)
    }
}

impl fmt::Debug for RegistryResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegistryResolver").finish_non_exhaustive()
    }
}

impl Resolver for RegistryResolver {
    fn advertise(&self, name: &str, topic: SIBinder) -> BinderResult<Advertisement> {
        let topic: Strong<dyn ITopic> = topic.into_interface()?;
        if let Err(status) = self.registry.r#advertise(name, &topic) {
            // A reply that failed on its way here leaves the name in the registry.
            if let Err(withdrawn) = self.registry.r#withdraw(name, &topic) {
                log::warn!("topic registry: withdrawing {name:?} failed: {withdrawn}");
            }
            return Err(status);
        }
        Ok(Advertisement::new(Advertised {
            registry: self.registry.clone(),
            name: name.to_owned(),
            topic,
        }))
    }

    fn watch(
        &self,
        name: &str,
        on_registered: Box<dyn Fn(SIBinder) + Send + Sync>,
    ) -> BinderResult<Watch> {
        let newest = Arc::new(Newest::new(on_registered));
        let watcher = BnTopicWatcher::new_binder(Notices {
            name: name.to_owned(),
            newest: newest.clone(),
        });
        let reply = match self.registry.r#watch(name, &watcher) {
            Ok(reply) => reply,
            Err(status) => {
                newest.end();
                // A reply that failed on its way here leaves the watch in the registry.
                let _ = self.registry.r#unwatch(name, &watcher);
                return Err(status);
            }
        };
        let (topic, generation) = parts(reply);
        newest.heard(topic, generation);
        let registry = self.registry.clone();
        let looked_up = name.to_owned();
        let answers = newest.clone();
        let lookup = move || {
            let (topic, generation) = parts(registry.r#lookup(&looked_up)?);
            Ok(answers.answer(topic, generation))
        };
        let watching = Watching {
            registry: self.registry.clone(),
            name: name.to_owned(),
            watcher,
            newest,
        };
        Ok(Watch::with_lookup(watching, lookup))
    }
}

fn parts(found: TopicRef) -> (Option<SIBinder>, i64) {
    (found.topic.map(|topic| topic.as_binder()), found.generation)
}

/// Withdraws the name when the advertisement is dropped.
struct Advertised {
    registry: Strong<dyn ITopicRegistry>,
    name: String,
    topic: Strong<dyn ITopic>,
}

impl Drop for Advertised {
    fn drop(&mut self) {
        // Oneway: it fails only when it cannot be sent, as the host has no room or died.
        if let Err(status) = self.registry.r#withdraw(&self.name, &self.topic) {
            log::warn!(
                "topic registry: withdrawing {:?} failed: {status}",
                self.name
            );
        }
    }
}

/// Ends the watch when it is dropped: the registry's, and the calls of notices still queued.
struct Watching {
    registry: Strong<dyn ITopicRegistry>,
    name: String,
    watcher: Strong<dyn ITopicWatcher>,
    newest: Arc<Newest<SIBinder>>,
}

impl Drop for Watching {
    fn drop(&mut self) {
        self.newest.end();
        // Oneway; a host that died has no watch left to end.
        let _ = self.registry.r#unwatch(&self.name, &self.watcher);
    }
}

/// The watcher binder of one watch.
struct Notices {
    name: String,
    newest: Arc<Newest<SIBinder>>,
}

impl Interface for Notices {}

impl ITopicWatcher for Notices {
    fn r#onAdvertised(
        &self,
        name: &str,
        topic: &Strong<dyn ITopic>,
        generation: i64,
    ) -> BinderResult<()> {
        if name == self.name {
            self.newest.heard(Some(topic.as_binder()), generation);
        }
        Ok(())
    }
}

/// The newest generation one watch has seen, and what it lets through (plan 15 D14-c).
pub(super) struct Newest<T> {
    /// The newest generation seen.
    seen: Mutex<i64>,
    /// Apart from `seen`: a callback that drops its own watch ends it without taking that lock.
    ended: AtomicBool,
    on_heard: Box<dyn Fn(T) + Send + Sync>,
}

impl<T> Newest<T> {
    pub(super) fn new(on_heard: Box<dyn Fn(T) + Send + Sync>) -> Self {
        Self {
            seen: Mutex::new(i64::MIN),
            ended: AtomicBool::new(false),
            on_heard,
        }
    }

    /// Whether `generation` is no older than any seen, which it then is the newest of.
    fn newer(&self, seen: &mut i64, generation: i64) -> bool {
        if self.ended.load(Ordering::Acquire) || generation < *seen {
            return false;
        }
        *seen = generation;
        true
    }

    /// A notice or the reply: passed on unless older, under the lock so the calls keep its order.
    pub(super) fn heard(&self, topic: Option<T>, generation: i64) {
        let mut seen = self.seen.lock().expect("lock poisoned");
        if self.newer(&mut seen, generation) {
            if let Some(topic) = topic {
                (self.on_heard)(topic);
            }
        }
    }

    /// A lookup's answer: `None` when older than one seen, as a notice since has the name's topic.
    pub(super) fn answer(&self, topic: Option<T>, generation: i64) -> Option<T> {
        let mut seen = self.seen.lock().expect("lock poisoned");
        self.newer(&mut seen, generation).then_some(topic).flatten()
    }

    /// Pass nothing on from now, without the lock: a running call, the caller's own too, goes on.
    fn end(&self) {
        self.ended.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    fn newest() -> (Newest<u32>, mpsc::Receiver<u32>) {
        let (send, heard) = mpsc::channel();
        let send = Mutex::new(send);
        let newest = Newest::new(Box::new(move |topic| {
            send.lock().unwrap().send(topic).unwrap();
        }));
        (newest, heard)
    }

    fn all(heard: &mpsc::Receiver<u32>) -> Vec<u32> {
        heard.try_iter().collect()
    }

    #[test]
    fn a_notice_older_than_one_seen_is_dropped_and_the_same_generation_passes() {
        let (newest, heard) = newest();
        newest.heard(Some(1), 5);
        newest.heard(Some(2), 3);
        newest.heard(Some(1), 5);
        newest.heard(Some(3), 6);
        assert_eq!(all(&heard), [1, 1, 3]);
    }

    #[test]
    fn a_name_with_no_topic_counts_as_seen() {
        let (newest, heard) = newest();
        newest.heard(None, 4);
        newest.heard(Some(1), 3);
        assert_eq!(all(&heard), []);
        assert_eq!(newest.answer(Some(2), 3), None);
        assert_eq!(newest.answer(None, 7), None);
        newest.heard(Some(3), 6);
        assert_eq!(all(&heard), []);
    }

    #[test]
    fn a_refused_answer_holds_back_no_later_answer() {
        let (newest, heard) = newest();
        newest.heard(Some(1), 5);
        assert_eq!(newest.answer(None, i64::MIN), None);
        assert_eq!(newest.answer(Some(2), 5), Some(2));
        newest.heard(Some(3), 5);
        assert_eq!(all(&heard), [1, 3]);
    }

    #[test]
    fn an_answer_older_than_a_notice_is_none_and_a_newer_one_holds_back_older_notices() {
        let (newest, heard) = newest();
        newest.heard(Some(1), 5);
        assert_eq!(newest.answer(Some(2), 4), None);
        assert_eq!(newest.answer(Some(3), 8), Some(3));
        newest.heard(Some(4), 7);
        assert_eq!(all(&heard), [1]);
        assert_eq!(newest.answer(Some(3), 8), Some(3));
    }

    #[test]
    fn nothing_passes_once_the_watch_ended() {
        let (newest, heard) = newest();
        newest.end();
        newest.heard(Some(1), 1);
        assert_eq!(newest.answer(Some(1), 2), None);
        assert_eq!(all(&heard), []);
    }

    /// A callback that ends its own watch, as by dropping the `Watch`, returns and ends it.
    #[test]
    fn a_callback_ends_its_own_watch() {
        let newest = Arc::new_cyclic(|weak: &std::sync::Weak<Newest<u32>>| {
            let weak = weak.clone();
            Newest::new(Box::new(move |_| {
                if let Some(newest) = weak.upgrade() {
                    newest.end();
                }
            }))
        });
        let (done, returned) = mpsc::channel();
        std::thread::spawn({
            let newest = newest.clone();
            move || {
                newest.heard(Some(1), 1);
                done.send(()).unwrap();
            }
        });
        assert!(returned
            .recv_timeout(std::time::Duration::from_secs(5))
            .is_ok());
        assert_eq!(newest.answer(Some(2), 2), None);
    }

    /// The callback runs under the lock: one that is running holds back the next call.
    #[test]
    fn calls_keep_the_order_of_their_generations() {
        let (entered, running) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let (record, recorded) = mpsc::channel();
        let entered = Mutex::new(entered);
        let released = Mutex::new(released);
        let record = Mutex::new(record);
        let newest = Arc::new(Newest::new(Box::new(move |topic: u32| {
            if topic == 1 {
                entered.lock().unwrap().send(()).unwrap();
                released.lock().unwrap().recv().unwrap();
            }
            record.lock().unwrap().send(topic).unwrap();
        })));
        let first = std::thread::spawn({
            let newest = newest.clone();
            move || newest.heard(Some(1), 1)
        });
        running.recv().unwrap();
        let second = std::thread::spawn({
            let newest = newest.clone();
            move || newest.heard(Some(2), 2)
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(newest.seen.try_lock().is_err());
        release.send(()).unwrap();
        first.join().unwrap();
        second.join().unwrap();
        assert_eq!(recorded.try_iter().collect::<Vec<_>>(), [1, 2]);
    }
}
