// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A registry of topic names (plan 15 §12): names apart from service names, given to topics
//! under a host's policy. **Experimental.**
//!
//! A [`Registry`] is the host side: a process that serves names, `rsb_hub` with its `topic`
//! feature, makes one with its policy ([`RegistryAccess`]) and registers its binder under
//! [`ITopicRegistry::SERVICE_NAME`]. The registry hands out topic binders and nothing more: a
//! subscription goes from the subscriber to the publisher as without it.
//!
//! A name is given to one topic at a time. The uid that advertised it may give it to another
//! topic, as a publisher that starts again does; another uid is refused while the topic lives,
//! unless the host allows it ([`Registry::allow_cross_uid_overwrite`]). A name is taken back when
//! its publisher withdraws it or its process dies.
//!
//! A watcher hears of every topic a name is given through a oneway call, which fails when the
//! watcher's process has no room left for one. The registry sends such a notice again, with the
//! topic the name has by then, for as long as its sends fail for want of room, the watch lasts,
//! the name has a topic and the policy allows the watch's caller: once a second at first, at
//! most once in 30 seconds after a few failures, and for at most 256 watches at a time. Once one
//! of these ends, the notice is not sent again.
//!
//! [`ITopicRegistry::SERVICE_NAME`]: crate::aidl::ITopicRegistry::SERVICE_NAME

mod table;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::Instant;

use rsbinder::{
    BinderResult, Caller, DeathRecipient, ExceptionCode, Interface, SIBinder, Status, StatusCode,
    Strong, WIBinder,
};

use crate::aidl::ITopic::ITopic;
use crate::aidl::ITopicRegistry::{BnTopicRegistry, ITopicRegistry, ERROR_LIMIT, ERROR_TAKEN};
use crate::aidl::ITopicWatcher::ITopicWatcher;
use crate::aidl::TopicInfo::TopicInfo;
use crate::aidl::TopicRef::TopicRef;
use crate::connect::refuse;
use table::{Key, Links, Notice, Refused, Sent, Table};

/// Decides what a caller may do with a name.
pub trait RegistryAccess: Send + Sync {
    /// Whether `caller` may do `action` on `name` (`""` for [`Action::List`]). An action the
    /// implementation does not know is refused.
    ///
    /// It must not call the registry. When it judges the watchers of a change it runs while the
    /// registry holds the lock that orders its notices, so its cost holds up the registry's other
    /// changes; every other judgement, those of notices sent again among them, runs with no lock
    /// of the registry held.
    fn allows(&self, action: Action, name: &str, caller: &Caller) -> bool;
}

/// What a caller asks to do with a name.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Give the name to a topic.
    Advertise,
    /// Find the name's topic, or watch it.
    Subscribe,
    /// List the names.
    List,
}

/// A [`RegistryAccess`] that allows everything, for tests and hosts without a policy.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAll;

impl RegistryAccess for AllowAll {
    fn allows(&self, _: Action, _: &str, _: &Caller) -> bool {
        true
    }
}

/// What a [`Registry`] holds and did.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RegistryStats {
    /// Names with a topic.
    pub topics: usize,
    /// Names with a watch.
    pub watched: usize,
    /// Watches over all names.
    pub watchers: usize,
    /// Notices of a change delivered.
    pub notices_sent: u64,
    /// Sends of a notice that failed, the first ones and the ones sent again.
    pub notices_failed: u64,
    /// Watches a notice is owed to, sent again until they take it.
    pub owed: usize,
    /// Sends of an owed notice.
    pub notices_retried: u64,
    /// Failed notices not kept to send again, as 256 were owed already.
    pub owed_dropped: u64,
}

/// Whether `name` is one a registry takes: 1 to 127 bytes of `[A-Za-z0-9._/-]`, as a service
/// name of `rsb_hub`.
pub fn is_valid_name(name: &str) -> bool {
    let allowed = |c: u8| c.is_ascii_alphanumeric() || b"._/-".contains(&c);
    (1..=127).contains(&name.len()) && name.bytes().all(allowed)
}

/// The host side of a registry of topic names: register [`as_binder`](Self::as_binder) with
/// the service manager under [`ITopicRegistry::SERVICE_NAME`].
///
/// Its callers are those of binder transactions, so a publisher or subscriber of the host
/// process cannot use it. A call made within the host process is no transaction: outside any
/// transaction it is refused, and within the handler of one it is judged as made by that
/// transaction's caller, which the registry does not support.
///
/// [`ITopicRegistry::SERVICE_NAME`]: crate::aidl::ITopicRegistry::SERVICE_NAME
pub struct Registry {
    shared: Arc<Shared>,
    binder: SIBinder,
}

impl Registry {
    /// A registry whose callers `access` judges.
    pub fn new(access: impl RegistryAccess + 'static) -> Self {
        let shared = Arc::new_cyclic(|this| Shared {
            access: Box::new(access),
            cross_uid: AtomicBool::new(false),
            fire_order: Mutex::new(()),
            inner: Mutex::new(Inner {
                table: Table::new(),
                linked: BTreeMap::new(),
                retrying: false,
            }),
            sleep: Arc::new(Sleep::default()),
            this: this.clone(),
            #[cfg(test)]
            hooks: Mutex::default(),
        });
        let binder = BnTopicRegistry::new_binder(Service(shared.clone())).as_binder();
        Self { shared, binder }
    }

    /// Let another uid give a name to its topic while the topic of the uid that holds it lives,
    /// as the hub's `--allow-cross-uid-overwrite` lets it register a service name.
    pub fn allow_cross_uid_overwrite(self, allow: bool) -> Self {
        self.shared.cross_uid.store(allow, Ordering::Relaxed);
        self
    }

    /// The registry's `ITopicRegistry` binder, the same every time.
    pub fn as_binder(&self) -> SIBinder {
        self.binder.clone()
    }

    /// What the registry holds and did.
    pub fn stats(&self) -> RegistryStats {
        let counts = self.shared.lock().table.counts();
        RegistryStats {
            topics: counts.topics,
            watched: counts.watched,
            watchers: counts.watchers,
            notices_sent: counts.sent,
            notices_failed: counts.failed,
            owed: counts.owed,
            notices_retried: counts.retried,
            owed_dropped: counts.dropped,
        }
    }
}

impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registry")
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

/// What the registry, its binder, its death recipients and its retry thread share.
struct Shared {
    access: Box<dyn RegistryAccess>,
    cross_uid: AtomicBool,
    /// Taken before `inner` and held while sending: notices leave in the order of their changes.
    fire_order: Mutex<()>,
    inner: Mutex<Inner>,
    /// Where the retry thread sleeps between rounds.
    sleep: Arc<Sleep>,
    this: Weak<Shared>,
    #[cfg(test)]
    hooks: Mutex<tests::Hooks>,
}

struct Inner {
    table: Table<TopicKey, WatcherKey>,
    /// The death link of each handle the table uses.
    linked: BTreeMap<u32, Linked>,
    /// A retry thread runs: it ends once nothing is owed.
    retrying: bool,
}

struct Linked {
    binder: SIBinder,
    obituary: Arc<Obituary>,
}

/// The retry thread's sleep: a change of its ticket ends it.
#[derive(Default)]
struct Sleep {
    ticket: Mutex<u64>,
    woken: Condvar,
}

impl Sleep {
    fn ticket(&self) -> u64 {
        *self.ticket.lock().expect("lock poisoned")
    }

    /// An earlier due, or the registry's end: the thread looks again.
    fn wake(&self) {
        *self.ticket.lock().expect("lock poisoned") += 1;
        self.woken.notify_all();
    }

    /// Sleep until `until` or a ticket other than `ticket`.
    fn until(&self, until: Instant, ticket: u64) {
        let mut held = self.ticket.lock().expect("lock poisoned");
        while *held == ticket {
            let now = Instant::now();
            if now >= until {
                break;
            }
            held = self
                .woken
                .wait_timeout(held, until - now)
                .expect("lock poisoned")
                .0;
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // The thread wakes to a registry it cannot upgrade, and ends.
        self.sleep.wake();
    }
}

#[derive(Clone)]
struct TopicKey(Strong<dyn ITopic>);

impl PartialEq for TopicKey {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_binder() == other.0.as_binder()
    }
}

impl Key for TopicKey {
    fn handle(&self) -> Option<u32> {
        handle_of(&self.0.as_binder())
    }
}

#[derive(Clone)]
struct WatcherKey(Strong<dyn ITopicWatcher>);

impl PartialEq for WatcherKey {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_binder() == other.0.as_binder()
    }
}

impl Key for WatcherKey {
    fn handle(&self) -> Option<u32> {
        handle_of(&self.0.as_binder())
    }
}

fn handle_of(binder: &SIBinder) -> Option<u32> {
    binder.as_proxy().map(|proxy| proxy.handle())
}

/// The links of a change, made under `inner`: `link_to_death` waits for no answer.
struct ShellLinks<'a> {
    linked: &'a mut BTreeMap<u32, Linked>,
    this: &'a Weak<Shared>,
}

impl ShellLinks<'_> {
    fn link(&mut self, handle: u32, binder: SIBinder) -> BinderResult<()> {
        let obituary = Arc::new(Obituary {
            handle,
            shared: self.this.clone(),
        });
        binder.link_to_death_arc(&obituary)?;
        self.linked.insert(handle, Linked { binder, obituary });
        Ok(())
    }
}

impl Links<TopicKey, WatcherKey> for ShellLinks<'_> {
    fn link_topic(&mut self, handle: u32, topic: &TopicKey) -> BinderResult<()> {
        self.link(handle, topic.0.as_binder())
    }

    fn link_watcher(&mut self, handle: u32, watcher: &WatcherKey) -> BinderResult<()> {
        self.link(handle, watcher.0.as_binder())
    }

    fn unlink(&mut self, handle: u32) {
        if let Some(linked) = self.linked.remove(&handle) {
            // A binder that died meanwhile has no link left to take back.
            let _ = linked.binder.unlink_to_death_arc(&linked.obituary);
        }
    }
}

/// The death of one handle the registry uses: its topics and watches go.
struct Obituary {
    handle: u32,
    shared: Weak<Shared>,
}

impl DeathRecipient for Obituary {
    fn binder_died(&self, _who: &WIBinder) {
        if let Some(shared) = self.shared.upgrade() {
            let mut inner = shared.lock();
            inner.table.forget(self.handle);
            let linked = inner.linked.remove(&self.handle);
            drop(inner);
            drop(linked);
        }
    }
}

/// The uid and pid of a kernel caller; an RPC caller has none the policy judges.
fn kernel_ids(caller: &Caller) -> Option<(u32, i32)> {
    match caller {
        Caller::Kernel { uid, pid, .. } => Some((*uid, *pid)),
        _ => None,
    }
}

fn security(why: &str) -> Status {
    Status::from((ExceptionCode::Security, why))
}

fn bad_name() -> Status {
    Status::from((ExceptionCode::IllegalArgument, "not a valid topic name"))
}

fn refused(refused: Refused) -> Status {
    match refused {
        Refused::Taken => refuse(ERROR_TAKEN, "the name is held by another uid's topic"),
        Refused::Limit | Refused::Full => {
            refuse(ERROR_LIMIT, "the registry holds as many as it takes")
        }
        Refused::Link(status) => status,
    }
}

fn topic_ref((topic, generation): (Option<TopicKey>, u64)) -> TopicRef {
    TopicRef {
        topic: topic.map(|topic| topic.0),
        generation: generation as i64,
    }
}

/// Send one notice; how it went, as [`classify`] judges a failure.
fn send(watcher: &WatcherKey, name: &str, topic: &TopicKey, generation: u64) -> Sent {
    match watcher.0.r#onAdvertised(name, &topic.0, generation as i64) {
        Ok(()) => Sent::Delivered,
        Err(status) => classify(&status, rsbinder::get_extended_error),
    }
}

/// What a failed send was, by its status and the kernel's detail of this thread's last failure.
fn classify(
    status: &Status,
    detail: impl FnOnce() -> rsbinder::Result<rsbinder::ExtendedError>,
) -> Sent {
    let failed = status.exception_code() == ExceptionCode::TransactionFailed
        && status.transaction_error() == StatusCode::FailedTransaction;
    if !failed {
        return Sent::Failed;
    }
    match detail() {
        // `param` is 0 when the driver recorded no detail.
        Ok(detail)
            if detail.param == 0 || detail.param == -rustix::io::Errno::NOSPC.raw_os_error() =>
        {
            Sent::Busy
        }
        Ok(_) => Sent::Failed,
        // A driver without extended errors.
        Err(_) => Sent::Busy,
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().expect("lock poisoned")
    }

    pub(crate) fn advertise_as(
        &self,
        caller: &Caller,
        name: &str,
        topic: &Strong<dyn ITopic>,
    ) -> BinderResult<()> {
        let ids = kernel_ids(caller).ok_or_else(|| security("a caller with no uid"))?;
        if !is_valid_name(name) {
            return Err(bad_name());
        }
        if !self.access.allows(Action::Advertise, name, caller) {
            return Err(security(
                "the policy does not let the caller advertise the name",
            ));
        }
        let cross_uid = self.cross_uid.load(Ordering::Relaxed);
        let order = self.fire_order.lock().expect("lock poisoned");
        let mut inner = self.lock();
        let Inner { table, linked, .. } = &mut *inner;
        let mut links = ShellLinks {
            linked,
            this: &self.this,
        };
        let key = TopicKey(topic.clone());
        let notices = table.advertise(ids, name, key, cross_uid, Instant::now(), &mut links);
        drop(inner);
        self.deliver(notices.map_err(refused)?);
        drop(order);
        Ok(())
    }

    pub(crate) fn withdraw_as(&self, caller: &Caller, name: &str, topic: &Strong<dyn ITopic>) {
        let Some((uid, _)) = kernel_ids(caller) else {
            return;
        };
        let mut inner = self.lock();
        let Inner { table, linked, .. } = &mut *inner;
        let mut links = ShellLinks {
            linked,
            this: &self.this,
        };
        table.withdraw(uid, name, &TopicKey(topic.clone()), &mut links);
    }

    pub(crate) fn lookup_as(&self, caller: &Caller, name: &str) -> TopicRef {
        let allowed = kernel_ids(caller).is_some()
            && is_valid_name(name)
            && self.access.allows(Action::Subscribe, name, caller);
        let inner = self.lock();
        // Refused, the answer of a name with no topic: the generation tells nothing of this one.
        let found = if allowed {
            inner.table.lookup(name)
        } else {
            (None, inner.table.generation())
        };
        topic_ref(found)
    }

    pub(crate) fn watch_as(
        &self,
        caller: &Caller,
        name: &str,
        watcher: &Strong<dyn ITopicWatcher>,
    ) -> BinderResult<TopicRef> {
        kernel_ids(caller).ok_or_else(|| security("a caller with no uid"))?;
        if !is_valid_name(name) {
            return Err(bad_name());
        }
        if !self.access.allows(Action::Subscribe, name, caller) {
            return Err(security(
                "the policy does not let the caller subscribe to the name",
            ));
        }
        let key = WatcherKey(watcher.clone());
        let watched = match self.try_watch(name, &key, caller) {
            Err(Refused::Full) => {
                // Full of watches: those whose callers lost the right since make room.
                let held = self.lock().table.callers_of(name);
                let refused = held
                    .into_iter()
                    .filter(|(_, held)| !self.access.allows(Action::Subscribe, name, held));
                let evicted: Vec<WatcherKey> = refused.map(|(key, _)| key).collect();
                self.unwatch_all(name, &evicted);
                self.try_watch(name, &key, caller)
            }
            other => other,
        };
        watched.map(topic_ref).map_err(refused)
    }

    fn try_watch(
        &self,
        name: &str,
        key: &WatcherKey,
        caller: &Caller,
    ) -> Result<(Option<TopicKey>, u64), Refused> {
        let mut inner = self.lock();
        let Inner { table, linked, .. } = &mut *inner;
        let mut links = ShellLinks {
            linked,
            this: &self.this,
        };
        table.watch(name, key.clone(), caller.clone(), &mut links)
    }

    pub(crate) fn unwatch_as(&self, name: &str, watcher: &Strong<dyn ITopicWatcher>) {
        self.unwatch_all(name, &[WatcherKey(watcher.clone())]);
    }

    fn unwatch_all(&self, name: &str, watchers: &[WatcherKey]) {
        let mut inner = self.lock();
        let Inner { table, linked, .. } = &mut *inner;
        let mut links = ShellLinks {
            linked,
            this: &self.this,
        };
        table.unwatch(name, watchers, &mut links);
    }

    pub(crate) fn list_as(&self, caller: &Caller, prefix: &str) -> BinderResult<Vec<TopicInfo>> {
        kernel_ids(caller).ok_or_else(|| security("a caller with no uid"))?;
        if !self.access.allows(Action::List, "", caller) {
            return Err(security(
                "the policy does not let the caller list the names",
            ));
        }
        let listed = self.lock().table.list(prefix);
        let allowed = listed
            .into_iter()
            .filter(|listed| self.access.allows(Action::Subscribe, &listed.name, caller));
        let infos = allowed.map(|listed| TopicInfo {
            name: listed.name,
            uid: listed.uid as i32,
            pid: listed.pid,
            ageMillis: i64::try_from(listed.since.elapsed().as_millis()).unwrap_or(i64::MAX),
            watchers: listed.watchers as i32,
        });
        Ok(infos.collect())
    }

    /// Judge and send the notices of a change, with `fire_order` held.
    fn deliver(&self, notices: Vec<Notice<TopicKey, WatcherKey>>) {
        for notice in notices {
            // Refused now, by a reload or a failed group lookup: the watch stays (plan 15 D4).
            if !self
                .access
                .allows(Action::Subscribe, &notice.name, &notice.caller)
            {
                continue;
            }
            #[cfg(test)]
            self.hook(|hooks| &hooks.before_send);
            let sent = send(
                &notice.watcher,
                &notice.name,
                &notice.topic,
                notice.generation,
            );
            let mut inner = self.lock();
            let now = Instant::now();
            let owed =
                inner
                    .table
                    .sent(&notice.name, &notice.watcher, notice.generation, sent, now);
            // Once per owed notice: a watcher that stays stuck fails every round.
            if owed || sent == Sent::Failed {
                log::warn!(
                    "topic registry: a notice of {:?} failed: {sent:?}",
                    notice.name
                );
            }
            self.owe(inner, owed);
        }
    }

    /// After a send under `inner`: start the retry thread, or wake it for an earlier due.
    fn owe(&self, mut inner: MutexGuard<'_, Inner>, added: bool) {
        if inner.retrying {
            if added {
                drop(inner);
                self.sleep.wake();
            }
            return;
        }
        // Owed with no thread only after a failed spawn: any send tries it again.
        if !added && inner.table.owed() == 0 {
            return;
        }
        inner.retrying = true;
        drop(inner);
        let shared = self.this.clone();
        let spawned = std::thread::Builder::new()
            .name("rsbinder-topic-registry".into())
            .spawn(move || retry(shared));
        if let Err(error) = spawned {
            log::warn!("topic registry: no thread to send owed notices again: {error}");
            self.lock().retrying = false;
        }
    }

    /// One round of the retry thread: every owed notice that is due, one send at a time.
    fn retry_round(&self) {
        let due = self.lock().table.due(Instant::now());
        for (name, watcher, caller, generation) in due {
            // Judged outside the locks: a slow group lookup holds up no change of the host's.
            if !self.access.allows(Action::Subscribe, &name, &caller) {
                // Kept if a newer notice failed meanwhile: that one is judged next round.
                self.lock().table.refused(&name, &watcher, generation);
                continue;
            }
            let order = self.fire_order.lock().expect("lock poisoned");
            let target = self.lock().table.retry_target(&name, &watcher);
            if let Some((topic, generation)) = target {
                let sent = send(&watcher, &name, &topic, generation);
                self.lock()
                    .table
                    .retried(&name, &watcher, generation, sent, Instant::now());
                if sent == Sent::Delivered {
                    log::info!("topic registry: an owed notice of {name:?} was delivered");
                }
            }
            drop(order);
            #[cfg(test)]
            self.hook(|hooks| &hooks.between_sends);
        }
    }

    #[cfg(test)]
    fn hook(&self, which: impl FnOnce(&tests::Hooks) -> &Option<tests::Hook>) {
        let hook = which(&self.hooks.lock().expect("lock poisoned")).clone();
        if let Some(hook) = hook {
            hook();
        }
    }
}

/// The retry thread: rounds until nothing is owed or the registry is gone.
fn retry(shared: Weak<Shared>) {
    #[cfg(test)]
    let _alive = shared
        .upgrade()
        .and_then(|s| s.hooks.lock().expect("lock poisoned").alive.clone());
    loop {
        let Some(registry) = shared.upgrade() else {
            return;
        };
        registry.retry_round();
        let inner = registry.lock();
        let Some(due) = inner.table.next_due() else {
            let mut inner = inner;
            inner.retrying = false;
            return;
        };
        // Read under `inner`, as the failures that wake it count under it.
        let ticket = registry.sleep.ticket();
        drop(inner);
        let sleep = registry.sleep.clone();
        drop(registry);
        sleep.until(due, ticket);
    }
}

/// The registry's binder: its callers are those of transactions.
struct Service(Arc<Shared>);

impl Interface for Service {}

/// The caller of the transaction in flight; with none in flight, the call is refused.
fn caller() -> BinderResult<Caller> {
    rsbinder::calling_caller().ok_or_else(|| security("not a binder transaction"))
}

impl ITopicRegistry for Service {
    fn r#advertise(&self, name: &str, topic: &Strong<dyn ITopic>) -> BinderResult<()> {
        self.0.advertise_as(&caller()?, name, topic)
    }

    fn r#withdraw(&self, name: &str, topic: &Strong<dyn ITopic>) -> BinderResult<()> {
        self.0.withdraw_as(&caller()?, name, topic);
        Ok(())
    }

    fn r#lookup(&self, name: &str) -> BinderResult<TopicRef> {
        Ok(self.0.lookup_as(&caller()?, name))
    }

    fn r#watch(&self, name: &str, watcher: &Strong<dyn ITopicWatcher>) -> BinderResult<TopicRef> {
        self.0.watch_as(&caller()?, name, watcher)
    }

    fn r#unwatch(&self, name: &str, watcher: &Strong<dyn ITopicWatcher>) -> BinderResult<()> {
        caller()?;
        self.0.unwatch_as(name, watcher);
        Ok(())
    }

    fn r#list(&self, prefix: &str) -> BinderResult<Vec<TopicInfo>> {
        self.0.list_as(&caller()?, prefix)
    }
}
