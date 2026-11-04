// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A registry of topic names over RPC on a Unix domain socket (plan 15-pubsub-uds §4.5, §4.6):
//! the same names, policy and refusals as [`Registry`](super::Registry), with no binder device.
//! **Experimental.**
//!
//! A binder cannot leave the RPC session it arrived on, so this registry hands out descriptors
//! instead. For each name it makes a socket in a directory of its own, binds it, opens its file
//! with `O_PATH` and unlinks it. The publisher is given the bound socket, on which it listens,
//! and the registry keeps no copy of it, so the socket ends when the publisher does. A subscriber
//! is given the `O_PATH` descriptor and connects through `/proc/self/fd/<n>`. With no name in
//! the file system, a socket is reached only by a process the registry gave a descriptor to, so
//! the host's `subscribe` policy says who can reach a topic. A process can pass its descriptor on
//! to another, as it can a kernel binder handle.
//!
//! The host side is [`UdsRegistry`], which `rsb_hub --topic-socket` serves; a program of its own
//! can serve one on any [`RpcServer`]. The client side is [`UdsResolver`], which
//! [`Publisher::advertise_with`](crate::Publisher::advertise_with) and
//! [`NamedSubscriber::subscribe_with`](crate::NamedSubscriber::subscribe_with) take.
//!
//! # What differs from a kernel registry
//!
//! A name lasts until it is withdrawn or the [`UdsResolver`] that advertised it has closed its
//! session with the host, which it does once it and everything made through it have dropped.
//! A process that ends takes its names with it. `/proc` has to be mounted.
//!
//! The file's mode bits are judged again at every connect, whatever the descriptor: they are
//! 0666, so what decides is the policy behind the descriptor. Root skips them.
//!
//! A change's notices go out as a [`Registry`](super::Registry)'s do: on the thread of the call
//! that made the change, in the order of the changes, each to a watcher with the name's generation
//! then. A send here takes one system call that waits for no peer, so a watcher that stopped holds
//! up no change and no other watcher. A notice with no room in the watcher's socket is owed, as one
//! is on a kernel registry when the watcher's process has no room: it is sent again, under the same
//! bounds, with the topic the name has by then, so a watcher that does not keep up skips
//! generations and never hears them out of order. The bound on how many are owed is the
//! registry's, whichever uid owes them (plan 15 D13-e): a uid allowed to `Subscribe` that stops
//! reading, or to `Advertise` and `Subscribe` as well and so change a name faster than it reads,
//! can take that room, and a notice past the bound goes unsent until the name's next change
//! (`owed_dropped`).
//!
//! The descriptors in notices and replies nobody has read count against the host's uid, and past
//! `RLIMIT_NOFILE` the kernel refuses more unless the host has `CAP_SYS_RESOURCE` or
//! `CAP_SYS_ADMIN`. A uid that can fill enough sockets can reach that: then every watcher's notice
//! has no room and is owed as above, and a reply with a descriptor fails its call with
//! `StatusCode::FailedTransaction`. Nothing waits and no other session ends. An `allocate` refused
//! that way leaves its name pending, as its caller has no number to withdraw, until the session
//! ends or allocates that name again.
//!
//! Unlike a kernel registry, a process that stopped with its socket full loses its session after
//! 10 s, which takes its names and watches away; one that goes on reading keeps it. A session that
//! could not be told at all, with no incoming connection or no descriptors to take, is refused its
//! watch with `InvalidOperation`.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use rsbinder::rpc::{PeerIdentity, RpcProxy, RpcServer};
use rsbinder::{
    BinderResult, Caller, DeathLink, DeathRecipient, ExceptionCode, IBinder, Interface,
    ParcelFileDescriptor, RemoteProxy, SIBinder, Status, StatusCode, Strong, TransportCaps,
    WIBinder,
};
use rustix::fs::{AtFlags, FileType, FlockOperation, Mode, OFlags};
use rustix::io::Errno;
use rustix::net::{AddressFamily, Shutdown, SocketAddrUnix, SocketFlags, SocketType};

use super::resolver::Newest;
use super::table::{Key, Links, Notice, Refused, Sent, Table};
use super::{
    is_valid_name, link_failed, no_allocation, Action, RegistryAccess, RegistryStats, Sleep,
};
use crate::aidl::ITopic::ITopic;
use crate::aidl::ITopicAddressWatcher::{
    transactions, BnTopicAddressWatcher, BpTopicAddressWatcher, ITopicAddressWatcher,
};
use crate::aidl::ITopicUdsRegistry::{
    BnTopicUdsRegistry, ITopicUdsRegistry, ERROR_LIMIT, ERROR_TAKEN,
};
use crate::aidl::TopicAddress::TopicAddress;
use crate::aidl::TopicAllocation::TopicAllocation;
use crate::aidl::TopicInfo::TopicInfo;
use crate::connect::refuse;
use crate::named::{FIRST_RETRY, LAST_RETRY};
use crate::registry::TopicListing;
use crate::resolve::{Advertisement, OnResolved, Resolved, Resolver, Watch};
use crate::uds::{self, Endpoint, Options, SessionGuard};
use crate::{Payload, Subscriber, SubscriberPolicy};

/// The directory a [`UdsRegistry`] makes its sockets in, held by a descriptor.
///
/// A socket's file lives there only between its bind and its unlink, a few system calls, so the
/// directory is empty unless a host crashed in between. Binding through the descriptor
/// (`/proc/self/fd/<n>/<file>`) means that the path's length is no limit, and that nothing can
/// swap the directory once it is checked.
#[derive(Debug)]
pub struct SocketDir {
    dir: OwnedFd,
    /// Names the next socket file: a number, not a topic's name, with no rule to follow.
    next: AtomicU64,
}

impl SocketDir {
    /// The directory at `path`, made with mode 0700 if it is not there, for this value alone.
    ///
    /// A directory is used only if it could only have been made for this: refused with
    /// `InvalidData` when it is a symlink or anything but a directory, when another uid than root
    /// or this process's owns it, when its mode is not 0700, when another `SocketDir` holds it,
    /// or when it holds anything but what a crash of a registry leaves, which is removed: a
    /// socket with a number for a name. The refusal names what it found, and removes nothing.
    pub fn open(path: &Path) -> io::Result<Self> {
        match rustix::fs::mkdir(path, Mode::RWXU) {
            Ok(()) => {}
            Err(Errno::EXIST) => {}
            Err(errno) => return Err(errno.into()),
        }
        let refused = |why: String| {
            let path = path.display();
            io::Error::new(io::ErrorKind::InvalidData, format!("{path}: {why}"))
        };
        let flags = OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::RDONLY | OFlags::CLOEXEC;
        let dir = rustix::fs::open(path, flags, Mode::empty()).map_err(|errno| match errno {
            Errno::LOOP | Errno::NOTDIR => refused("a symlink, or not a directory".to_owned()),
            errno => errno.into(),
        })?;
        let stat = rustix::fs::fstat(&dir)?;
        let own = rustix::process::geteuid().as_raw();
        if stat.st_uid != 0 && stat.st_uid != own {
            return Err(refused(format!(
                "owned by uid {}, neither root nor this process",
                stat.st_uid
            )));
        }
        if stat.st_mode & 0o777 != 0o700 {
            return Err(refused(format!(
                "mode {:o}, not 700",
                stat.st_mode & 0o7777
            )));
        }
        match rustix::fs::flock(&dir, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(Errno::WOULDBLOCK) => return Err(refused("in use by another host".to_owned())),
            Err(errno) => return Err(errno.into()),
        }
        let this = Self {
            dir,
            next: AtomicU64::new(0),
        };
        this.clear(refused)?;
        Ok(this)
    }

    /// Remove what a crash between a bind and its unlink left, if that is all there is.
    fn clear(&self, refused: impl Fn(String) -> io::Error) -> io::Result<()> {
        let mut left = Vec::new();
        for entry in rustix::fs::Dir::read_from(&self.dir)? {
            let entry = entry?;
            let name = entry.file_name();
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }
            let stat = rustix::fs::statat(&self.dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
            let socket = FileType::from_raw_mode(stat.st_mode) == FileType::Socket;
            if !socket || !name.to_bytes().iter().all(u8::is_ascii_digit) {
                return Err(refused(format!(
                    "it holds {name:?}, which no registry made"
                )));
            }
            left.push(name.to_owned());
        }
        for name in left {
            if let Err(errno) = rustix::fs::unlinkat(&self.dir, &name, AtFlags::empty()) {
                log::warn!("topic registry: cannot remove {name:?} from its directory: {errno}");
            }
        }
        Ok(())
    }

    /// A socket bound to a new file, mode 0666, and an `O_PATH` descriptor of the file, unlinked.
    fn bind(&self) -> io::Result<(OwnedFd, OwnedFd)> {
        let serial = self.next.fetch_add(1, Ordering::Relaxed);
        let file = serial.to_string();
        let socket = rustix::net::socket_with(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC,
            None,
        )?;
        let path = format!("/proc/self/fd/{}/{file}", self.dir.as_raw_fd());
        rustix::net::bind(&socket, &SocketAddrUnix::new(path.as_str())?)?;
        let removal = |error: io::Error| {
            let _ = rustix::fs::unlinkat(&self.dir, file.as_str(), AtFlags::empty());
            error
        };
        let mode = Mode::from_raw_mode(0o666);
        rustix::fs::chmodat(&self.dir, file.as_str(), mode, AtFlags::empty())
            .map_err(|errno| removal(errno.into()))?;
        let flags = OFlags::PATH | OFlags::CLOEXEC;
        let address = rustix::fs::openat(&self.dir, file.as_str(), flags, Mode::empty())
            .map_err(|errno| removal(errno.into()))?;
        rustix::fs::unlinkat(&self.dir, file.as_str(), AtFlags::empty())?;
        Ok((socket, address))
    }
}

/// A topic's binder on the session of its publisher, and which allocation of a name it is for.
#[derive(Clone)]
struct TopicKey {
    /// Told apart by proxy: another session cannot make this one (plan 15-pubsub-uds F27).
    topic: SIBinder,
    allocation: u64,
    /// The `O_PATH` descriptor of the socket; `None` in a key that only names an allocation.
    address: Option<Arc<OwnedFd>>,
}

impl PartialEq for TopicKey {
    fn eq(&self, other: &Self) -> bool {
        self.allocation == other.allocation && self.topic == other.topic
    }
}

impl Key for TopicKey {
    fn handle(&self) -> Option<u64> {
        handle_of(&self.topic)
    }
}

#[derive(Clone)]
struct WatcherKey(Strong<dyn ITopicAddressWatcher>);

impl PartialEq for WatcherKey {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_binder() == other.0.as_binder()
    }
}

impl Key for WatcherKey {
    fn handle(&self) -> Option<u64> {
        handle_of(&self.0.as_binder())
    }
}

/// What tells this proxy from another while the key holds it: where it is, as `Arc::ptr_eq` sees.
fn handle_of(binder: &SIBinder) -> Option<u64> {
    let proxy: &dyn IBinder = &**binder;
    Some(std::ptr::from_ref(proxy).cast::<()>() as usize as u64)
}

/// The links of a change, made under `inner`: a proxy's link and its last drop wait for no peer.
struct ShellLinks<'a> {
    linked: &'a mut BTreeMap<u64, Linked>,
    links_made: &'a mut u64,
    this: &'a Weak<Shared>,
}

/// One handle's link; `serial` tells it from a link at the same address with a notice on its way.
struct Linked {
    serial: u64,
    _link: DeathLink,
}

impl ShellLinks<'_> {
    fn link(&mut self, handle: u64, binder: &SIBinder) -> BinderResult<()> {
        *self.links_made += 1;
        let serial = *self.links_made;
        let obituary = Arc::new(Obituary {
            handle,
            serial,
            shared: self.this.clone(),
        });
        let link = DeathLink::new(binder, obituary)?;
        let linked = Linked {
            serial,
            _link: link,
        };
        self.linked.insert(handle, linked);
        Ok(())
    }
}

impl Links<TopicKey, WatcherKey> for ShellLinks<'_> {
    fn link_topic(&mut self, handle: u64, topic: &TopicKey) -> BinderResult<()> {
        self.link(handle, &topic.topic)
    }

    fn link_watcher(&mut self, handle: u64, watcher: &WatcherKey) -> BinderResult<()> {
        self.link(handle, &watcher.0.as_binder())
    }

    fn unlink(&mut self, handle: u64) {
        self.linked.remove(&handle);
    }
}

/// The end of a session a binder came on: its names, allocations and watches go.
struct Obituary {
    handle: u64,
    serial: u64,
    shared: Weak<Shared>,
}

impl DeathRecipient for Obituary {
    fn binder_died(&self, _who: &WIBinder) {
        let Some(shared) = self.shared.upgrade() else {
            return;
        };
        let mut inner = shared.lock();
        if inner.linked.get(&self.handle).map(|l| l.serial) != Some(self.serial) {
            return;
        }
        inner.table.forget(self.handle);
        let linked = inner.linked.remove(&self.handle);
        drop(inner);
        drop(linked);
    }
}

struct Inner {
    table: Table<TopicKey, WatcherKey>,
    linked: BTreeMap<u64, Linked>,
    links_made: u64,
    /// A retry thread runs: it ends once nothing is owed.
    retrying: bool,
}

struct Shared {
    access: Box<dyn RegistryAccess>,
    dir: SocketDir,
    cross_uid: AtomicBool,
    next_allocation: AtomicU64,
    /// Taken before `inner` and held while sending: notices leave in the order of their changes.
    fire_order: Mutex<()>,
    inner: Mutex<Inner>,
    /// Where the retry thread sleeps between rounds.
    sleep: Arc<Sleep>,
    this: Weak<Shared>,
    /// Duplicates of an address the host fails from now on, as with no descriptor left.
    #[cfg(test)]
    failing_duplicates: AtomicU64,
    /// A copy of the last address made, for a test to connect through one nobody can look up.
    #[cfg(test)]
    last_address: Mutex<Option<OwnedFd>>,
    /// The next retry round panics, for a test.
    #[cfg(test)]
    panicking_retry: AtomicBool,
}

impl Drop for Shared {
    fn drop(&mut self) {
        // The thread wakes to a registry it cannot upgrade, and ends.
        self.sleep.wake();
    }
}

fn caller() -> BinderResult<Caller> {
    rsbinder::calling_caller().ok_or_else(|| security("not a binder transaction"))
}

/// The uid and pid of a caller over a Unix socket, which `SO_PEERCRED` gave.
fn local_ids(caller: &Caller) -> Option<(u32, i32)> {
    match caller {
        Caller::Rpc(PeerIdentity::Local { uid, pid, .. }) => Some((*uid, *pid)),
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
        Refused::Link(status) => link_failed(&status),
        Refused::NoAllocation => no_allocation(),
    }
}

/// `address` as a descriptor for a parcel; a failed duplicate is the error of the call that asked.
fn parcel_address(address: &OwnedFd) -> BinderResult<ParcelFileDescriptor> {
    let dup = address.as_fd().try_clone_to_owned();
    Ok(ParcelFileDescriptor::new(dup.map_err(StatusCode::from)?))
}

fn topic_address((topic, generation): (Option<TopicKey>, u64)) -> BinderResult<TopicAddress> {
    let address = topic.and_then(|topic| topic.address);
    Ok(TopicAddress {
        socket: address
            .map(|address| parcel_address(&address))
            .transpose()?,
        generation: generation as i64,
    })
}

/// The oneway `onAdvertised` with no wait for room, which the stub's call would wait for.
pub(super) fn try_notice(
    watcher: &Strong<dyn ITopicAddressWatcher>,
    name: &str,
    address: &ParcelFileDescriptor,
    generation: u64,
) -> rsbinder::Result<()> {
    let binder = watcher.as_binder();
    let Some(proxy) = binder.as_any().downcast_ref::<RpcProxy>() else {
        return Err(StatusCode::InvalidOperation);
    };
    let mut data =
        proxy.prepare_request(<BpTopicAddressWatcher as ITopicAddressWatcher>::descriptor())?;
    data.write(name)?;
    data.write(address)?;
    data.write(&(generation as i64))?;
    let flags = rsbinder::FLAG_ONEWAY | rsbinder::FLAG_CLEAR_BUF | rsbinder::FLAG_PRIVATE_LOCAL;
    proxy.__try_oneway(transactions::r#onAdvertised, &data, flags)
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().expect("lock poisoned")
    }

    /// `fire_order` guards no data: a panic under it leaves nothing to repair.
    fn order(&self) -> MutexGuard<'_, ()> {
        self.fire_order
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn cross_uid(&self) -> bool {
        self.cross_uid.load(Ordering::Relaxed)
    }

    fn allocate(
        &self,
        caller: &Caller,
        name: &str,
        topic: &Strong<dyn ITopic>,
    ) -> BinderResult<TopicAllocation> {
        let (uid, _) = local_ids(caller).ok_or_else(|| security("a caller with no uid"))?;
        if !is_valid_name(name) {
            return Err(bad_name());
        }
        if !self.access.allows(Action::Advertise, name, caller) {
            return Err(security(
                "the policy does not let the caller advertise the name",
            ));
        }
        let (socket, address) = self.dir.bind().map_err(StatusCode::from)?;
        let allocation = self.next_allocation.fetch_add(1, Ordering::Relaxed);
        #[cfg(test)]
        {
            *self.last_address.lock().expect("lock poisoned") = address.try_clone().ok();
        }
        let key = TopicKey {
            topic: topic.as_binder(),
            allocation,
            address: Some(Arc::new(address)),
        };
        let cross_uid = self.cross_uid();
        let mut inner = self.lock();
        let Inner {
            table,
            linked,
            links_made,
            ..
        } = &mut *inner;
        let mut links = ShellLinks {
            linked,
            links_made,
            this: &self.this,
        };
        table
            .allocate(uid, name, key, cross_uid, &mut links)
            .map_err(refused)?;
        drop(inner);
        // The parcel holds its own duplicate; this descriptor closes with the return.
        Ok(TopicAllocation {
            socket: Some(ParcelFileDescriptor::new(socket)),
            allocation: allocation as i64,
        })
    }

    fn named_key(topic: &Strong<dyn ITopic>, allocation: i64) -> TopicKey {
        TopicKey {
            topic: topic.as_binder(),
            allocation: allocation as u64,
            address: None,
        }
    }

    fn commit(
        &self,
        caller: &Caller,
        name: &str,
        topic: &Strong<dyn ITopic>,
        allocation: i64,
    ) -> BinderResult<i64> {
        let ids = local_ids(caller).ok_or_else(|| security("a caller with no uid"))?;
        let key = Self::named_key(topic, allocation);
        // A reload may have taken the right since the allocation; the refusal ends it, as any.
        if !self.access.allows(Action::Advertise, name, caller) {
            self.withdraw_pending(ids.0, name, &key);
            return Err(security(
                "the policy does not let the caller advertise the name",
            ));
        }
        let cross_uid = self.cross_uid();
        let order = self.order();
        let mut inner = self.lock();
        let Inner {
            table,
            linked,
            links_made,
            ..
        } = &mut *inner;
        let mut links = ShellLinks {
            linked,
            links_made,
            this: &self.this,
        };
        let change = table.commit(ids, name, &key, cross_uid, Instant::now(), &mut links);
        drop(inner);
        let (notices, generation) = change.map_err(refused)?;
        self.deliver(notices);
        drop(order);
        Ok(generation as i64)
    }

    fn withdraw_pending(&self, uid: u32, name: &str, key: &TopicKey) {
        let mut inner = self.lock();
        let Inner {
            table,
            linked,
            links_made,
            ..
        } = &mut *inner;
        let mut links = ShellLinks {
            linked,
            links_made,
            this: &self.this,
        };
        table.withdraw_pending(uid, name, key, &mut links);
    }

    fn withdraw(&self, caller: &Caller, name: &str, topic: &Strong<dyn ITopic>, allocation: i64) {
        let Some((uid, _)) = local_ids(caller) else {
            return;
        };
        let key = Self::named_key(topic, allocation);
        let mut inner = self.lock();
        let Inner {
            table,
            linked,
            links_made,
            ..
        } = &mut *inner;
        let mut links = ShellLinks {
            linked,
            links_made,
            this: &self.this,
        };
        table.withdraw(uid, name, &key, &mut links);
        table.withdraw_pending(uid, name, &key, &mut links);
    }

    fn lookup(&self, caller: &Caller, name: &str) -> BinderResult<TopicAddress> {
        let allowed = local_ids(caller).is_some()
            && is_valid_name(name)
            && self.access.allows(Action::Subscribe, name, caller);
        if !allowed {
            return Ok(TopicAddress {
                socket: None,
                generation: i64::MIN,
            });
        }
        let found = self.lock().table.lookup(name);
        topic_address(found)
    }

    fn watch(
        &self,
        caller: &Caller,
        name: &str,
        watcher: &Strong<dyn ITopicAddressWatcher>,
    ) -> BinderResult<TopicAddress> {
        local_ids(caller).ok_or_else(|| security("a caller with no uid"))?;
        if !is_valid_name(name) {
            return Err(bad_name());
        }
        if !self.access.allows(Action::Subscribe, name, caller) {
            return Err(security(
                "the policy does not let the caller subscribe to the name",
            ));
        }
        // Each notice would fail the same until the session ended, holding a place among the owed.
        let needed = TransportCaps::CALLBACKS | TransportCaps::FD_PASSING;
        let caps = rsbinder::calling_caps().unwrap_or(TransportCaps::NONE);
        caps.require(needed, "a watch of a topic's name")?;
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
        topic_address(watched.map_err(refused)?)
    }

    fn try_watch(
        &self,
        name: &str,
        key: &WatcherKey,
        caller: &Caller,
    ) -> Result<(Option<TopicKey>, u64), Refused> {
        let mut inner = self.lock();
        let Inner {
            table,
            linked,
            links_made,
            ..
        } = &mut *inner;
        let mut links = ShellLinks {
            linked,
            links_made,
            this: &self.this,
        };
        table.watch(name, key.clone(), caller.clone(), &mut links)
    }

    fn unwatch_all(&self, name: &str, watchers: &[WatcherKey]) {
        let mut inner = self.lock();
        let Inner {
            table,
            linked,
            links_made,
            ..
        } = &mut *inner;
        let mut links = ShellLinks {
            linked,
            links_made,
            this: &self.this,
        };
        table.unwatch(name, watchers, &mut links);
    }

    fn list(&self, caller: &Caller, prefix: &str) -> BinderResult<Vec<TopicInfo>> {
        local_ids(caller).ok_or_else(|| security("a caller with no uid"))?;
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
            if !self.judged(&notice.name, &notice.caller) {
                continue;
            }
            let sent = self.send(
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

    /// Whether the policy lets `caller` hear of `name` now; a judgement that panics did not.
    fn judged(&self, name: &str, caller: &Caller) -> bool {
        let judged = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.access.allows(Action::Subscribe, name, caller)
        }));
        judged.unwrap_or_else(|payload| {
            let panic = rsbinder::__panic_message(&payload);
            log::error!("topic registry: a judgement of {name:?} panicked ({panic})");
            false
        })
    }

    /// One send: final if it would fail the same while the session lives, or the session ended.
    fn send(&self, watcher: &WatcherKey, name: &str, topic: &TopicKey, generation: u64) -> Sent {
        let Some(address) = topic.address.as_deref() else {
            return Sent::Failed;
        };
        let address = parcel_address(address);
        #[cfg(test)]
        let address = if self.failing_duplicates.load(Ordering::Relaxed) > 0 {
            self.failing_duplicates.fetch_sub(1, Ordering::Relaxed);
            Err(StatusCode::from(io::Error::from_raw_os_error(libc::EMFILE)).into())
        } else {
            address
        };
        let Ok(address) = address else {
            return Sent::Busy;
        };
        match try_notice(&watcher.0, name, &address, generation) {
            Ok(()) => Sent::Delivered,
            Err(
                StatusCode::DeadObject
                | StatusCode::FdsNotAllowed
                | StatusCode::InvalidOperation
                | StatusCode::BadValue,
            ) => Sent::Failed,
            Err(_) => Sent::Busy,
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
        // Owed with no thread after a failed spawn: a later send starts one.
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

    /// One round of the retry thread: every owed notice that is due, with the name's topic by now.
    fn retry_round(&self) {
        #[cfg(test)]
        assert!(
            !self.panicking_retry.swap(false, Ordering::SeqCst),
            "the retry round's panic"
        );
        let due = self.lock().table.due(Instant::now());
        for (name, watcher, caller, generation) in due {
            if !self.judged(&name, &caller) {
                self.lock().table.refused(&name, &watcher, generation);
                continue;
            }
            let order = self.order();
            let target = self.lock().table.retry_target(&name, &watcher);
            if let Some((topic, generation)) = target {
                let sent = self.send(&watcher, &name, &topic, generation);
                self.lock()
                    .table
                    .retried(&name, &watcher, generation, sent, Instant::now());
            }
            drop(order);
        }
    }
}

/// Clears `retrying` when the retry thread panics: left set, no later send would start another.
struct Ending(Weak<Shared>);

impl Drop for Ending {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        if let Some(shared) = self.0.upgrade() {
            let mut inner = shared.inner.lock().unwrap_or_else(PoisonError::into_inner);
            inner.retrying = false;
        }
    }
}

/// The retry thread: rounds until nothing is owed or the registry is gone.
fn retry(shared: Weak<Shared>) {
    let _ending = Ending(shared.clone());
    loop {
        let Some(registry) = shared.upgrade() else {
            return;
        };
        registry.retry_round();
        let mut inner = registry.lock();
        let Some(due) = inner.table.next_due() else {
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

/// The host side of a registry of topic names over a Unix socket: serve
/// [`as_binder`](Self::as_binder) as the root of an [`RpcServer`] set up with
/// [`serve`](Self::serve).
pub struct UdsRegistry {
    shared: Arc<Shared>,
    binder: SIBinder,
}

/// How long a send to a watcher may go without progress: `RpcServer`'s default handshake deadline.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Watch addresses of one session with an unpaid notice: past what a socket buffer of 12 MB queues.
const MAX_UNPAID_WATCHES: usize = 10_000;

impl UdsRegistry {
    /// A registry whose callers `access` judges and whose sockets are made in `dir`.
    pub fn new(dir: SocketDir, access: impl RegistryAccess + 'static) -> Self {
        let shared = Arc::new_cyclic(|this| Shared {
            access: Box::new(access),
            dir,
            cross_uid: AtomicBool::new(false),
            next_allocation: AtomicU64::new(1),
            fire_order: Mutex::new(()),
            inner: Mutex::new(Inner {
                table: Table::new(),
                linked: BTreeMap::new(),
                links_made: 0,
                retrying: false,
            }),
            sleep: Arc::new(Sleep::default()),
            this: this.clone(),
            #[cfg(test)]
            panicking_retry: AtomicBool::new(false),
            #[cfg(test)]
            failing_duplicates: AtomicU64::new(0),
            #[cfg(test)]
            last_address: Mutex::default(),
        });
        let binder = BnTopicUdsRegistry::new_binder(Service(shared.clone())).as_binder();
        Self { shared, binder }
    }

    /// Let another uid give a name to its topic while the topic of the uid that holds it lives.
    pub fn allow_cross_uid_overwrite(self, allow: bool) -> Self {
        self.shared.cross_uid.store(allow, Ordering::Relaxed);
        self
    }

    /// The registry's `ITopicUdsRegistry` binder, the same every time.
    pub fn as_binder(&self) -> SIBinder {
        self.binder.clone()
    }

    /// Set `server` up to serve this registry, which then runs when the caller runs it: the
    /// settings a client needs, a deadline on what it sends to a watcher that stopped, and no
    /// descriptor taken from a client ([`RpcServer::set_receive_fds`]), as no call has one.
    ///
    /// The server's serve workers are the caller's to bound
    /// ([`RpcServer::set_max_connections`]). A session is one worker, up to two callback
    /// connections with no thread of their own, and two descriptors more in a call: five
    /// descriptors. A worker is taken before the policy judges any call on it, so a socket any uid
    /// can connect to needs a bound that leaves the descriptors of [`MAX_NAMES`](super::MAX_NAMES)
    /// free.
    ///
    /// A client that takes its notices and does not pay them back, watching a new address each
    /// time, would have the registry keep an entry for each until its session ends. The session
    /// ends at 10 000 such addresses instead. A client paying for its notices stays far under
    /// that, as what it has not paid for is what its socket buffer holds, and an `rsbinder-topic`
    /// client pays on the connection the registry reads. A libbinder client pays on an idle
    /// outgoing connection too, and on the callback connection, which the registry does not read,
    /// when it has none idle: such payments count toward the limit.
    pub fn serve(&self, server: &RpcServer) -> BinderResult<()> {
        uds::configure(server);
        server.set_receive_fds(false);
        server.__limit_unpaid_addresses(MAX_UNPAID_WATCHES);
        server.set_root(self.as_binder())?;
        server.set_reply_timeout(Some(SEND_TIMEOUT))?;
        Ok(())
    }

    /// Start a retry thread, as a send that owes does, whose first round panics, for a test.
    #[cfg(test)]
    pub(super) fn start_a_panicking_retry(&self) {
        self.shared.panicking_retry.store(true, Ordering::SeqCst);
        self.shared.owe(self.shared.lock(), true);
    }

    /// Whether a retry thread has not run the round [`Self::start_a_panicking_retry`] armed.
    #[cfg(test)]
    pub(super) fn retry_round_armed(&self) -> bool {
        self.shared.panicking_retry.load(Ordering::SeqCst)
    }

    /// Whether a retry thread counts as running, for a test.
    #[cfg(test)]
    pub(super) fn retrying(&self) -> bool {
        self.shared.lock().retrying
    }

    /// Fail the next `count` duplicates of an address, as with no descriptor left, for a test.
    #[cfg(test)]
    pub(super) fn fail_duplicates(&self, count: u64) {
        self.shared
            .failing_duplicates
            .store(count, Ordering::Relaxed);
    }

    /// What asks the registry's state from another thread than a fixture's, for a test.
    #[cfg(test)]
    pub(super) fn shared_for_a_test(&self) -> Self {
        Self {
            shared: self.shared.clone(),
            binder: self.binder.clone(),
        }
    }

    /// The address of the last allocation, whatever became of it, for a test.
    #[cfg(test)]
    pub(super) fn last_address(&self) -> Option<OwnedFd> {
        self.shared
            .last_address
            .lock()
            .expect("lock poisoned")
            .take()
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
            pending: counts.pending,
        }
    }
}

impl fmt::Debug for UdsRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UdsRegistry")
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

/// The registry's binder.
struct Service(Arc<Shared>);

impl Interface for Service {}

impl ITopicUdsRegistry for Service {
    fn r#allocate(&self, name: &str, topic: &Strong<dyn ITopic>) -> BinderResult<TopicAllocation> {
        self.0.allocate(&caller()?, name, topic)
    }

    fn r#commit(
        &self,
        name: &str,
        topic: &Strong<dyn ITopic>,
        allocation: i64,
    ) -> BinderResult<i64> {
        self.0.commit(&caller()?, name, topic, allocation)
    }

    fn r#withdraw(
        &self,
        name: &str,
        topic: &Strong<dyn ITopic>,
        allocation: i64,
    ) -> BinderResult<()> {
        self.0.withdraw(&caller()?, name, topic, allocation);
        Ok(())
    }

    fn r#lookup(&self, name: &str) -> BinderResult<TopicAddress> {
        self.0.lookup(&caller()?, name)
    }

    fn r#watch(
        &self,
        name: &str,
        watcher: &Strong<dyn ITopicAddressWatcher>,
    ) -> BinderResult<TopicAddress> {
        self.0.watch(&caller()?, name, watcher)
    }

    fn r#unwatch(
        &self,
        name: &str,
        watcher: &Strong<dyn ITopicAddressWatcher>,
    ) -> BinderResult<()> {
        caller()?;
        self.0.unwatch_all(name, &[WatcherKey(watcher.clone())]);
        Ok(())
    }

    fn r#list(&self, prefix: &str) -> BinderResult<Vec<TopicInfo>> {
        self.0.list(&caller()?, prefix)
    }
}

/// How a [`UdsResolver`] connects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolverOptions {
    /// The bound on each step of connecting, to the registry and to each publisher, and on the
    /// subscription call of [`UdsResolver::subscribe`] ([`Options::timeout`]). Default 10 s.
    ///
    /// A [`NamedSubscriber`](crate::NamedSubscriber)'s subscription call has no bound, as over
    /// kernel binder: past a deadline a session ends, where a publisher that goes on again
    /// answers the call. One that stopped in the call holds that one connection until it goes
    /// on or ends, or its session closes, as the name going to another publisher or the
    /// subscriber's drop closes it.
    pub timeout: Option<Duration>,
    /// The uid the registry's host has to be ([`Options::expect_uid`]). Default `None`.
    pub expect_host_uid: Option<u32>,
    /// The uid every publisher a name is given to has to be: a second check beside the host's
    /// policy, for a host that was set up wrong. A publisher of another uid is not subscribed
    /// to, and a [`NamedSubscriber`](crate::NamedSubscriber) says so with `EX_SECURITY`, as a
    /// publisher's refusal. Default `None`.
    pub expect_publisher_uid: Option<u32>,
    /// The serve workers the server of a topic this resolver advertises runs at once
    /// ([`RpcServer::set_max_connections`]); a subscriber's session takes one, and a second while
    /// its other connection attaches. A session is two descriptors and two threads of the
    /// publisher, and up to 64 descriptors more while its peer holds back the rest of a frame it
    /// sent them with, as a subscription call takes one. A process that holds the topic's address
    /// can make as many as this lets it. Default `None`: as many as it asks for.
    pub publisher_connections: Option<usize>,
}

impl Default for ResolverOptions {
    fn default() -> Self {
        Self {
            timeout: Options::default().timeout,
            expect_host_uid: None,
            expect_publisher_uid: None,
            publisher_connections: None,
        }
    }
}

/// The session with the host, closed once the resolver and all it made have let go of it.
struct Host {
    registry: Strong<dyn ITopicUdsRegistry>,
    _session: SessionGuard,
    /// How a connection to a publisher goes.
    publisher: Options,
    /// The bound on the connections of a topic this resolver serves.
    publisher_connections: Option<usize>,
}

/// A [`UdsRegistry`] as a [`Resolver`]: names under the policy of its host, with no binder device.
///
/// One value is one session with the host. `advertise` serves the topic on a socket the host
/// makes, and the [`Advertisement`] ends both: its drop stops accepting, so that a connect to the
/// socket is refused, and withdraws the name from a thread of its own, so that a host that
/// stopped does not hold the drop. That leaves the sessions of subscribers already connected, so
/// that they read the topic's end record; they close as those subscribers end, or with this
/// process. A [`Publisher`](crate::Publisher) holds one advertisement, so it is on one registry
/// at a time, kernel or over a socket. `advertise_on` with any resolver replaces the one it has;
/// `rsbinder::hub::add_service` of its binder takes none and goes with it.
///
/// `watch` makes one session for each generation of the name, on a thread of the watch's, so one
/// publisher that is slow to accept holds up only its own name's watch. A connect into a publisher
/// whose accept queue is full waits in `connect(2)` until it accepts or ends: that thread is held
/// that long, and [`ResolverOptions::timeout`] does not reach it.
///
/// A connection that fails for any other reason is made again, once a second at first and at
/// most once in 30 seconds, until it is made, the name has a newer generation or the watch ends.
/// Two failures are final, as the same socket gives the same answer: a publisher of another uid
/// than [`ResolverOptions::expect_publisher_uid`], and a socket nobody listens on any more.
///
/// Two resolvers of one host have different host binders, so
/// [`Publisher::advertise_on`](crate::Publisher::advertise_on) asks again when given the other:
/// it replaces its own advertisement, which does no harm.
///
/// A host that ends takes its names and watches with it, as a kernel registry's does: connected
/// subscribers go on, and a [`NamedSubscriber`](crate::NamedSubscriber) returns
/// [`Closed::ResolverDied`](crate::Closed::ResolverDied) once it has no connection left.
#[derive(Clone)]
pub struct UdsResolver {
    host: Arc<Host>,
}

impl UdsResolver {
    /// A session with the host at `endpoint`, typically a socket path
    /// (`/run/rsbinder/topic.sock`, or `$XDG_RUNTIME_DIR/rsbinder/topic.sock` for a host of a
    /// login session).
    pub fn connect(endpoint: Endpoint, options: &ResolverOptions) -> BinderResult<Self> {
        let host_options = Options {
            timeout: options.timeout,
            expect_uid: options.expect_host_uid,
        };
        let session = uds::connect(endpoint, &host_options)?;
        let registry = session.0.get_root()?.into_interface_checked()?;
        // A name lasts as long as this session, which a deadline running out would end.
        session.0.set_reply_timeout(None)?;
        Ok(Self {
            host: Arc::new(Host {
                registry,
                _session: session,
                publisher: Options {
                    timeout: options.timeout,
                    expect_uid: options.expect_publisher_uid,
                },
                publisher_connections: options.publisher_connections,
            }),
        })
    }

    /// The names under `prefix` with a topic that this process may subscribe to, sorted, as the
    /// registry sends them. `EX_SECURITY` when the host's policy does not let this process list.
    pub fn list(&self, prefix: &str) -> BinderResult<Vec<TopicListing>> {
        let listed = self.host.registry.r#list(prefix)?;
        Ok(listed.into_iter().map(TopicListing::from).collect())
    }

    /// A subscription to the topic `name` has now: `NameNotFound` when it has none, or the host's
    /// policy does not let this process subscribe to it. It holds a session of its own and goes
    /// on after this resolver drops.
    pub fn subscribe<T: Payload>(
        &self,
        name: &str,
        policy: &SubscriberPolicy,
    ) -> BinderResult<Subscriber<T>> {
        let (address, _) = parts(self.host.registry.r#lookup(name)?);
        let address = address.ok_or(StatusCode::NameNotFound)?;
        uds::subscribe_with(Endpoint::Fd(address), policy, &self.host.publisher)
    }

    /// How many answers `name` gets when a notice is lost just after the first, for a test.
    #[cfg(test)]
    pub(super) fn answers_when_a_notice_is_lost_during_the_first(&self, name: &str) -> u64 {
        let asking = Arc::new(Mutex::new(Asking {
            running: true,
            again: false,
        }));
        let answers = Arc::new(AtomicU64::new(0));
        let (counted, losing) = (answers.clone(), asking.clone());
        let newest = Newest::new(Box::new(move |_: (OwnedFd, i64)| {
            if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                losing.lock().expect("lock poisoned").again = true;
            }
        }));
        let dialer = Dialer::new(Options::default(), Box::new(|_| {}));
        let host = Arc::downgrade(&self.host);
        ask_until_answered(name, &newest, &dialer, &host, &asking);
        answers.load(Ordering::SeqCst)
    }

    fn serve(&self, socket: OwnedFd, topic: SIBinder) -> BinderResult<Serving> {
        let listener = socket.try_clone().map_err(StatusCode::from)?;
        let server = RpcServer::setup_unix_server_from_bound_fd(socket)?;
        uds::configure(&server);
        if let Some(connections) = self.host.publisher_connections {
            server.set_max_connections(connections);
        }
        server.set_root(topic)?;
        let running = server.clone();
        let accepting = std::thread::Builder::new()
            .name("rsbinder-topic-accept".into())
            .spawn(move || {
                if let Err(error) = running.run() {
                    log::warn!("topic: the publisher's accept loop ended: {error}");
                }
            });
        accepting.map_err(|_| StatusCode::NoMemory)?;
        Ok(Serving { server, listener })
    }
}

impl fmt::Debug for UdsResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UdsResolver").finish_non_exhaustive()
    }
}

/// A topic's server, and its listening socket apart from it to refuse connects with.
struct Serving {
    server: Arc<RpcServer>,
    listener: OwnedFd,
}

impl Serving {
    /// The server keeps its listener while a session lives, where a connect would wait unanswered.
    fn stop(&self) {
        self.server.stop_accepting();
        if let Err(errno) = rustix::net::shutdown(&self.listener, Shutdown::Read) {
            log::debug!("topic: the publisher's socket still takes connects: {errno}");
        }
    }
}

/// A call on a thread of its own: even a oneway one waits for the connection a stopped host holds.
fn off_this_thread(host: &Arc<Host>, call: impl FnOnce(&Host) + Send + 'static) {
    let host = host.clone();
    let spawned = std::thread::Builder::new()
        .name("rsbinder-topic-host".into())
        .spawn(move || call(&host));
    if let Err(error) = spawned {
        log::warn!("topic registry: no thread for a call; the session's end makes it: {error}");
    }
}

/// Withdraws the name when the advertisement is dropped, and stops taking new subscribers.
struct Advertised {
    host: Arc<Host>,
    serving: Serving,
    name: String,
    topic: Strong<dyn ITopic>,
    allocation: i64,
}

impl Drop for Advertised {
    fn drop(&mut self) {
        self.serving.stop();
        let name = std::mem::take(&mut self.name);
        let topic = self.topic.clone();
        let allocation = self.allocation;
        off_this_thread(&self.host, move |host| {
            if let Err(status) = host.registry.r#withdraw(&name, &topic, allocation) {
                log::warn!("topic registry: withdrawing {name:?} failed: {status}");
            }
        });
    }
}

impl Resolver for UdsResolver {
    /// `BadValue` when `topic` is a remote binder: a server serves one of this process's own.
    fn advertise(&self, name: &str, topic: SIBinder) -> BinderResult<Advertisement> {
        if topic.is_remote() {
            return Err(StatusCode::BadValue.into());
        }
        let typed: Strong<dyn ITopic> = topic.clone().into_interface()?;
        let TopicAllocation { socket, allocation } = self.host.registry.r#allocate(name, &typed)?;
        let served = socket
            .ok_or_else(|| Status::from(StatusCode::UnexpectedNull))
            .and_then(|socket| self.serve(OwnedFd::from(socket), topic));
        let serving = match served {
            Ok(serving) => serving,
            Err(status) => {
                if let Err(withdrawn) = self.host.registry.r#withdraw(name, &typed, allocation) {
                    log::warn!("topic registry: withdrawing {name:?} failed: {withdrawn}");
                }
                return Err(status);
            }
        };
        if let Err(status) = self.host.registry.r#commit(name, &typed, allocation) {
            // The host dropped the allocation with its refusal, or with this session's end.
            serving.stop();
            return Err(status);
        }
        Ok(Advertisement::new(Advertised {
            host: self.host.clone(),
            serving,
            name: name.to_owned(),
            topic: typed,
            allocation,
        }))
    }

    fn watch(
        &self,
        name: &str,
        on_registered: Box<dyn Fn(SIBinder) + Send + Sync>,
    ) -> BinderResult<Watch> {
        let on_resolved = Box::new(move |resolved: BinderResult<Resolved>| {
            if let Ok(resolved) = resolved {
                on_registered(resolved.into_binder());
            }
        });
        self.watch_resolved(name, on_resolved)
    }

    fn watch_resolved(&self, name: &str, on_resolved: OnResolved) -> BinderResult<Watch> {
        let dialer = Arc::new(Dialer::new(self.host.publisher.clone(), on_resolved));
        let dialing = dialer.clone();
        let newest = Arc::new(Newest::new(Box::new(
            move |(address, generation): (OwnedFd, i64)| dialing.give(address, generation),
        )));
        let watcher = BnTopicAddressWatcher::new_binder(Notices {
            name: name.to_owned(),
            newest: newest.clone(),
            dialer: dialer.clone(),
            // Weak: the host's session keeps this watcher until the host pays for it.
            host: Arc::downgrade(&self.host),
            asking: Arc::default(),
        });
        let registry = &self.host.registry;
        let reply = match registry.r#watch(name, &watcher) {
            Ok(reply) => reply,
            Err(status) => {
                newest.end();
                dialer.end();
                let _ = registry.r#unwatch(name, &watcher);
                return Err(status);
            }
        };
        let (address, generation) = parts(reply);
        newest.heard(address.map(|address| (address, generation)), generation);
        let host = self.host.clone();
        let looked_up = name.to_owned();
        let answers = newest.clone();
        let asked = dialer.clone();
        let lookup = move || {
            let (address, generation) = parts(host.registry.r#lookup(&looked_up)?);
            let found = answers.answer(address.map(|address| (address, generation)), generation);
            match found {
                Some((address, generation)) => asked.dial_for_lookup(address, generation),
                None => Ok(None),
            }
        };
        let watching = Watching {
            host: self.host.clone(),
            name: name.to_owned(),
            watcher,
            newest,
            dialer,
        };
        Ok(Watch::with_resolved_lookup(watching, lookup))
    }

    fn host(&self) -> Option<SIBinder> {
        Some(self.host.registry.as_binder())
    }
}

fn parts(address: TopicAddress) -> (Option<OwnedFd>, i64) {
    (address.socket.map(OwnedFd::from), address.generation)
}

/// Ends the watch when it is dropped: the host's, and the calls of notices still on their way.
struct Watching {
    host: Arc<Host>,
    name: String,
    watcher: Strong<dyn ITopicAddressWatcher>,
    newest: Arc<Newest<(OwnedFd, i64)>>,
    dialer: Arc<Dialer>,
}

impl Drop for Watching {
    fn drop(&mut self) {
        self.newest.end();
        self.dialer.end();
        let name = std::mem::take(&mut self.name);
        let watcher = self.watcher.clone();
        off_this_thread(&self.host, move |host| {
            let _ = host.registry.r#unwatch(&name, &watcher);
        });
    }
}

/// The watcher binder of one watch.
struct Notices {
    name: String,
    newest: Arc<Newest<(OwnedFd, i64)>>,
    dialer: Arc<Dialer>,
    host: Weak<Host>,
    asking: Arc<Mutex<Asking>>,
}

/// The thread that asks the host for a notice whose descriptor could not be kept.
#[derive(Default)]
struct Asking {
    running: bool,
    /// Another notice was lost after what its call asks may have been answered: it asks again.
    again: bool,
}

impl Interface for Notices {}

impl ITopicAddressWatcher for Notices {
    fn r#onAdvertised(
        &self,
        name: &str,
        socket: &ParcelFileDescriptor,
        generation: i64,
    ) -> BinderResult<()> {
        if name != self.name {
            return Ok(());
        }
        match self.duplicate(socket) {
            Ok(address) => self.newest.heard(Some((address, generation)), generation),
            Err(error) => {
                log::warn!("topic: no descriptor for a notice of {name:?}, asking again: {error}");
                self.ask_again();
            }
        }
        Ok(())
    }
}

impl Notices {
    fn duplicate(&self, socket: &ParcelFileDescriptor) -> std::io::Result<OwnedFd> {
        #[cfg(test)]
        if FAILING_NOTICES.lock().expect("lock poisoned").as_deref() == Some(self.name.as_str()) {
            *FAILING_NOTICES.lock().expect("lock poisoned") = None;
            return Err(Errno::MFILE.into());
        }
        socket.as_ref().try_clone()
    }

    /// The oneway has no one to fail to, and the host counts it delivered: look the name up.
    fn ask_again(&self) {
        let mut asking = self.asking.lock().expect("lock poisoned");
        if asking.running {
            asking.again = true;
            return;
        }
        asking.running = true;
        drop(asking);
        let name = self.name.clone();
        let newest = self.newest.clone();
        let dialer = self.dialer.clone();
        let host = self.host.clone();
        let asking = self.asking.clone();
        let spawned = std::thread::Builder::new()
            .name("rsbinder-topic-host".into())
            .spawn(move || {
                ask_until_answered(&name, &newest, &dialer, &host, &asking);
                asking.lock().expect("lock poisoned").running = false;
            });
        if let Err(error) = spawned {
            log::warn!("topic: no thread to ask for {:?} again: {error}", self.name);
            self.asking.lock().expect("lock poisoned").running = false;
        }
    }
}

/// Look `name` up until the last notice lost has its answer, or the watch or the host is gone.
fn ask_until_answered(
    name: &str,
    newest: &Newest<(OwnedFd, i64)>,
    dialer: &Dialer,
    host: &Weak<Host>,
    asking: &Mutex<Asking>,
) {
    let mut delay = FIRST_RETRY;
    while !dialer.ended_within(delay) {
        // A reply's fd with no slot for it ends the session: wait for that and one to dial with.
        if let Err(error) = std::os::unix::net::UnixStream::pair() {
            log::debug!("topic: no descriptors to ask for {name:?} yet: {error}");
            delay = (delay * 2).min(LAST_RETRY);
            continue;
        }
        let Some(host) = host.upgrade() else {
            return;
        };
        asking.lock().expect("lock poisoned").again = false;
        match host.registry.r#lookup(name) {
            Ok(reply) => {
                let (address, generation) = parts(reply);
                newest.heard(address.map(|address| (address, generation)), generation);
                if !asking.lock().expect("lock poisoned").again {
                    return;
                }
                delay = FIRST_RETRY;
            }
            Err(status) => {
                log::debug!("topic: asking for {name:?}: {status}");
                delay = (delay * 2).min(LAST_RETRY);
            }
        }
    }
}

/// The name whose next notice fails its duplicate, as with no descriptor left, for a test.
#[cfg(test)]
pub(super) static FAILING_NOTICES: Mutex<Option<String>> = Mutex::new(None);

/// The connection of the newest generation one watch has reached.
struct Reached {
    generation: i64,
    resolved: Resolved,
    /// A session that ended answers for its generation no more: the publisher may live on.
    session: Arc<SessionGuard>,
}

#[derive(Default)]
struct DialQueue {
    next: Option<(OwnedFd, i64)>,
    /// The newest generation given: one older, a lookup's overtaken by a notice, goes unused.
    given: Option<i64>,
    running: bool,
    ended: bool,
    /// The connection of the newest generation reached, for the repeats of it, until the end.
    reached: Option<Reached>,
}

/// A generation to connect to again.
struct Again {
    address: OwnedFd,
    generation: i64,
    at: Instant,
}

/// Clears `running` when the connecting thread unwinds; starts another for a notice given since.
struct Dialing<'d>(&'d Arc<Dialer>);

impl Drop for Dialing<'_> {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        // No caller code runs under this lock, so a poisoned one is no callback's: leave it.
        let next = match self.0.queue.lock() {
            Ok(mut queue) => {
                queue.running = false;
                queue.next.take()
            }
            Err(poisoned) => {
                poisoned.into_inner().running = false;
                return;
            }
        };
        if let Some((address, generation)) = next {
            self.0.give(address, generation);
        }
    }
}

/// Whether another connect to the same socket can answer otherwise (see [`UdsResolver`]).
fn may_connect_again(status: &Status) -> bool {
    let refused = StatusCode::Errno(-Errno::CONNREFUSED.raw_os_error());
    status.exception_code() != ExceptionCode::Security && status.transaction_error() != refused
}

/// One watch's descriptors to connections, off the host session's thread, which all names share.
pub(super) struct Dialer {
    options: Options,
    on_resolved: OnResolved,
    queue: Mutex<DialQueue>,
    /// Wakes a thread waiting to connect again.
    changed: Condvar,
    /// Held across a connect and its call, so lookups and notices keep the generations' order.
    order: Mutex<()>,
}

impl Dialer {
    pub(super) fn new(options: Options, on_resolved: OnResolved) -> Self {
        Self {
            options,
            on_resolved,
            queue: Mutex::default(),
            changed: Condvar::new(),
            order: Mutex::default(),
        }
    }

    /// A notice or the watch's reply: connect to the newest of them, the older go unused.
    pub(super) fn give(self: &Arc<Self>, address: OwnedFd, generation: i64) {
        let mut queue = self.queue.lock().expect("lock poisoned");
        if queue.ended || queue.given.is_some_and(|given| given > generation) {
            return;
        }
        queue.given = Some(generation);
        queue.next = Some((address, generation));
        self.changed.notify_all();
        if queue.running {
            return;
        }
        queue.running = true;
        drop(queue);
        let dialer = self.clone();
        let spawned = std::thread::Builder::new()
            .name("rsbinder-topic-dial".into())
            .spawn(move || dialer.run());
        if let Err(error) = spawned {
            log::warn!("topic: no thread to connect to the name's publisher: {error}");
            self.queue.lock().expect("lock poisoned").running = false;
        }
    }

    /// What to connect to: a generation given since, or `again` once its time comes.
    fn next(&self, again: &mut Option<Again>) -> Option<(OwnedFd, i64)> {
        let mut queue = self.queue.lock().expect("lock poisoned");
        loop {
            if queue.ended {
                queue.running = false;
                return None;
            }
            if let Some(next) = queue.next.take() {
                *again = None;
                return Some(next);
            }
            let Some(at) = again.as_ref().map(|again| again.at) else {
                queue.running = false;
                return None;
            };
            let Some(wait) = at.checked_duration_since(Instant::now()) else {
                return again.take().map(|again| (again.address, again.generation));
            };
            queue = self
                .changed
                .wait_timeout(queue, wait)
                .expect("lock poisoned")
                .0;
        }
    }

    /// `order` guards no data: a callback's panic under it leaves nothing to repair.
    fn order(&self) -> MutexGuard<'_, ()> {
        self.order.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn run(self: Arc<Self>) {
        let _dialing = Dialing(&self);
        let mut again = None;
        let mut tried = None;
        let mut delay = FIRST_RETRY;
        while let Some((address, generation)) = self.next(&mut again) {
            if tried != Some(generation) {
                tried = Some(generation);
                delay = FIRST_RETRY;
            }
            let _order = self.order();
            match self.reach(&address, generation) {
                Ok(Some(resolved)) => {
                    if !self.queue.lock().expect("lock poisoned").ended {
                        (self.on_resolved)(Ok(resolved));
                    }
                }
                Ok(None) => {}
                Err(status) => {
                    log::debug!("topic: cannot reach the name's publisher: {status}");
                    if may_connect_again(&status) {
                        let at = Instant::now() + delay;
                        delay = (delay * 2).min(LAST_RETRY);
                        again = Some(Again {
                            address,
                            generation,
                            at,
                        });
                    }
                }
            }
        }
    }

    /// The connection of `generation`, made here unless it exists; `None` for an older one.
    pub(super) fn dial(
        &self,
        address: &OwnedFd,
        generation: i64,
    ) -> BinderResult<Option<Resolved>> {
        let _order = self.order();
        self.reach(address, generation)
    }

    /// Until `delay` passes or the dialer ends; whether it ended.
    fn ended_within(&self, delay: Duration) -> bool {
        let queue = self.queue.lock().expect("lock poisoned");
        let (queue, _) = self
            .changed
            .wait_timeout_while(queue, delay, |queue| !queue.ended)
            .expect("lock poisoned");
        queue.ended
    }

    /// [`dial`](Self::dial) for a lookup, whose caller gives up on a failure: the thread goes on.
    fn dial_for_lookup(
        self: &Arc<Self>,
        address: OwnedFd,
        generation: i64,
    ) -> BinderResult<Option<Resolved>> {
        let reached = self.dial(&address, generation);
        if reached.as_ref().is_err_and(may_connect_again) {
            self.give(address, generation);
        }
        reached
    }

    /// Under `order`; the queue's lock only between the steps, as [`Self::end`] waits for it.
    fn reach(&self, address: &OwnedFd, generation: i64) -> BinderResult<Option<Resolved>> {
        {
            let queue = self.queue.lock().expect("lock poisoned");
            if queue.ended {
                return Ok(None);
            }
            match queue.reached.as_ref() {
                Some(reached) if reached.generation > generation => return Ok(None),
                Some(reached)
                    if reached.generation == generation && !reached.session.0.is_ended() =>
                {
                    return Ok(Some(reached.resolved.clone()));
                }
                _ => {}
            }
        }
        let address = address.try_clone().map_err(StatusCode::from)?;
        let session = uds::connect(Endpoint::Fd(address), &self.options)
            .inspect_err(|status| self.tell_if_refused(status))?;
        let binder = session.0.get_root()?;
        session.0.set_reply_timeout(None)?;
        let session = Arc::new(session);
        let resolved = Resolved::with_session(binder, session.clone());
        let mut queue = self.queue.lock().expect("lock poisoned");
        if queue.ended {
            drop(queue);
            return Ok(None);
        }
        let replaced = queue.reached.replace(Reached {
            generation,
            resolved: resolved.clone(),
            session,
        });
        drop(queue);
        drop(replaced);
        Ok(Some(resolved))
    }

    /// Tell the watch of a publisher its check refused, which no connect to that socket changes.
    fn tell_if_refused(&self, status: &Status) {
        if status.exception_code() != ExceptionCode::Security {
            return;
        }
        log::warn!("topic: the name's publisher is refused: {status}");
        if !self.queue.lock().expect("lock poisoned").ended {
            (self.on_resolved)(Err(status.clone()));
        }
    }

    /// Stop connecting, and let go of the connection, which a call in it may be held in for good.
    pub(super) fn end(&self) {
        let mut queue = self.queue.lock().expect("lock poisoned");
        queue.ended = true;
        queue.next = None;
        let reached = queue.reached.take();
        self.changed.notify_all();
        drop(queue);
        drop(reached);
    }

    /// No thread of this dialer runs and none is to, for a test.
    #[cfg(test)]
    pub(super) fn idle(&self) -> bool {
        let queue = self.queue.lock().expect("lock poisoned");
        !queue.running && queue.next.is_none()
    }
}
