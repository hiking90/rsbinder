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
//! bounds, with the generation the name has by then, so a watcher that does not keep up skips
//! generations and never hears them out of order. The bound on how many are owed is the
//! registry's, whichever uid owes them (plan 15 D13-e): a uid allowed to `Subscribe` that stops
//! reading, or to `Advertise` and `Subscribe` as well and so change a name faster than it reads,
//! can take that room, and a notice past the bound goes unsent until the name's next change
//! (`owed_dropped`).
//!
//! A notice says that a name changed and the watch asks for what it has now, with `lookup`, so a
//! notice carries no descriptor. A process with no descriptor free takes it and asks later, once
//! it has one: a reply that brings a descriptor to such a process fails, as the kernel cannot
//! deliver it (`MSG_CTRUNC`). The watch's asking can still lose that race to another thread of the
//! process; that reply fails alone, and the watch asks again.
//!
//! The descriptors in replies nobody has read count against the host's uid, and past
//! `RLIMIT_NOFILE` the kernel refuses more unless the host has `CAP_SYS_RESOURCE` or
//! `CAP_SYS_ADMIN`. A uid that can fill enough sockets can reach that: a reply with a descriptor
//! then fails its call with `StatusCode::FailedTransaction`. Nothing waits and no other session
//! ends. An `allocate` refused that way leaves its name pending, as its caller has no number to
//! withdraw, until the session ends or allocates that name again.
//!
//! Unlike a kernel registry, a process that stopped with its socket full loses its session after
//! 10 s, which takes its names and watches away; one that goes on reading keeps it. A session that
//! could not be told, with no incoming connection, or could not ask, as it takes no descriptors, is
//! refused its watch with `InvalidOperation`.

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
    /// Notices the host finds no room for from now on.
    #[cfg(test)]
    failing_sends: AtomicU64,
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
    generation: u64,
) -> rsbinder::Result<()> {
    let binder = watcher.as_binder();
    let Some(proxy) = binder.as_any().downcast_ref::<RpcProxy>() else {
        return Err(StatusCode::InvalidOperation);
    };
    let mut data =
        proxy.prepare_request(<BpTopicAddressWatcher as ITopicAddressWatcher>::descriptor())?;
    data.write(name)?;
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
    ) -> BinderResult<i64> {
        local_ids(caller).ok_or_else(|| security("a caller with no uid"))?;
        if !is_valid_name(name) {
            return Err(bad_name());
        }
        if !self.access.allows(Action::Subscribe, name, caller) {
            return Err(security(
                "the policy does not let the caller subscribe to the name",
            ));
        }
        // It could not be told, or not take the descriptor a lookup replies with: refused now.
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
        let (_, generation) = watched.map_err(refused)?;
        Ok(generation as i64)
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
            let sent = self.send(&notice.watcher, &notice.name, notice.generation);
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
    fn send(&self, watcher: &WatcherKey, name: &str, generation: u64) -> Sent {
        #[cfg(test)]
        if self.failing_sends.load(Ordering::Relaxed) > 0 {
            self.failing_sends.fetch_sub(1, Ordering::Relaxed);
            return Sent::Busy;
        }
        match try_notice(&watcher.0, name, generation) {
            Ok(()) => Sent::Delivered,
            Err(StatusCode::DeadObject | StatusCode::InvalidOperation | StatusCode::BadValue) => {
                Sent::Failed
            }
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

    /// One round of the retry thread: every owed notice due, with the name's generation by now.
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
            if let Some((_, generation)) = target {
                let sent = self.send(&watcher, &name, generation);
                self.lock()
                    .table
                    .retried(&name, &watcher, generation, sent, Instant::now());
            }
            drop(order);
        }
    }
}

/// The retry thread: rounds until nothing is owed or the registry is gone.
fn retry(shared: Weak<Shared>) {
    loop {
        let Some(registry) = shared.upgrade() else {
            return;
        };
        // A panic costs one round: what is owed stays owed, with this thread to send it.
        let round = std::panic::AssertUnwindSafe(|| registry.retry_round());
        let panicked = std::panic::catch_unwind(round).map_err(|payload| {
            let panic = rsbinder::__panic_message(&payload);
            log::error!("topic registry: a round of owed notices panicked ({panic})");
        });
        let mut inner = registry.lock();
        let Some(due) = inner.table.next_due() else {
            inner.retrying = false;
            return;
        };
        // What the round did not reach is due at once: no spin on a panic that comes back.
        let due = match panicked {
            Ok(()) => due,
            Err(()) => due.max(Instant::now() + FIRST_RETRY),
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

/// A [`UdsResolver`] has one watcher binder, so its notices have one address to pay for.
const WATCH_ADDRESSES: usize = 1;

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
            failing_sends: AtomicU64::new(0),
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
    /// time, would have the registry keep an entry for each until its session ends. A client has
    /// one watcher binder for all its names, as [`UdsResolver`] does, so a session ends at a notice
    /// to a second address while the first has one unpaid, however it pays.
    pub fn serve(&self, server: &RpcServer) -> BinderResult<()> {
        uds::configure(server);
        server.set_receive_fds(false);
        server.__limit_unpaid_addresses(WATCH_ADDRESSES);
        server.set_root(self.as_binder())?;
        server.set_reply_timeout(Some(SEND_TIMEOUT))?;
        Ok(())
    }

    /// Make the next round of the retry thread panic, for a test.
    #[cfg(test)]
    pub(super) fn panic_in_the_next_retry_round(&self) {
        self.shared.panicking_retry.store(true, Ordering::SeqCst);
    }

    /// Whether a retry thread counts as running, for a test.
    #[cfg(test)]
    pub(super) fn retrying(&self) -> bool {
        self.shared.lock().retrying
    }

    /// Find no room for the next `count` notices, for a test.
    #[cfg(test)]
    pub(super) fn fail_sends(&self, count: u64) {
        self.shared.failing_sends.store(count, Ordering::Relaxed);
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

    fn r#watch(&self, name: &str, watcher: &Strong<dyn ITopicAddressWatcher>) -> BinderResult<i64> {
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
    /// The one binder the host tells of every name this resolver watches.
    watcher: Strong<dyn ITopicAddressWatcher>,
    watches: Arc<Watches>,
    /// Across a `watch` and an `unwatch`: the host's watch of a name is all of its watches here.
    watch_order: Mutex<()>,
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
/// The host tells this resolver when a name changes, and a thread of the watch's asks it what the
/// name has now. That thread runs while there is something to ask, one a watch at most.
///
/// `watch` makes one session for each generation of the name, on that thread, so one
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
        let watches = Arc::new(Watches::default());
        Ok(Self {
            host: Arc::new(Host {
                registry,
                watcher: BnTopicAddressWatcher::new_binder(Notices(watches.clone())),
                watches,
                watch_order: Mutex::default(),
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

    /// A watch the host knows nothing of: a test tells it of a notice with `heard`.
    #[cfg(test)]
    pub(super) fn reconciler(&self, name: &str, on_resolved: OnResolved) -> Arc<Reconciler> {
        Arc::new(Reconciler::new(
            name,
            Arc::downgrade(&self.host),
            self.host.publisher.clone(),
            on_resolved,
        ))
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
        let host = &self.host;
        let reconciler = Arc::new(Reconciler::new(
            name,
            Arc::downgrade(host),
            host.publisher.clone(),
            on_resolved,
        ));
        let order = host.watch_order();
        // Before the call: a notice can come ahead of its reply.
        host.watches.add(name, &reconciler);
        if let Err(status) = host.registry.r#watch(name, &host.watcher) {
            reconciler.end();
            host.forget(name, &reconciler);
            return Err(status);
        }
        drop(order);
        // A name may have its socket already, and no notice comes of that.
        reconciler.heard();
        let asked = reconciler.clone();
        let watching = Watching {
            host: host.clone(),
            name: name.to_owned(),
            reconciler,
        };
        Ok(Watch::with_resolved_lookup(watching, move || {
            asked.lookup()
        }))
    }

    fn host(&self) -> Option<SIBinder> {
        Some(self.host.registry.as_binder())
    }
}

fn parts(address: TopicAddress) -> (Option<OwnedFd>, i64) {
    (address.socket.map(OwnedFd::from), address.generation)
}

impl Host {
    /// `watch_order` guards no data: a panic under it leaves nothing to repair.
    fn watch_order(&self) -> MutexGuard<'_, ()> {
        self.watch_order
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Take a watch off its name, and the host's watch with the name's last; under the order.
    fn forget(&self, name: &str, watch: &Arc<Reconciler>) {
        if self.watches.remove(name, watch) {
            let _ = self.registry.r#unwatch(name, &self.watcher);
        }
    }
}

/// Ends the watch when it is dropped: the host's, and what it was asking.
struct Watching {
    host: Arc<Host>,
    name: String,
    reconciler: Arc<Reconciler>,
}

impl Drop for Watching {
    fn drop(&mut self) {
        self.reconciler.end();
        let name = std::mem::take(&mut self.name);
        let reconciler = self.reconciler.clone();
        off_this_thread(&self.host, move |host| {
            let _order = host.watch_order();
            host.forget(&name, &reconciler);
        });
    }
}

/// The watches of one resolver, by name.
#[derive(Default)]
struct Watches(Mutex<BTreeMap<String, Vec<Weak<Reconciler>>>>);

impl Watches {
    fn add(&self, name: &str, watch: &Arc<Reconciler>) {
        let mut names = self.0.lock().expect("lock poisoned");
        let watches = names.entry(name.to_owned()).or_default();
        watches.push(Arc::downgrade(watch));
    }

    /// Take `watch` off `name`; whether the name has none left.
    fn remove(&self, name: &str, watch: &Arc<Reconciler>) -> bool {
        let mut names = self.0.lock().expect("lock poisoned");
        let Some(watches) = names.get_mut(name) else {
            return true;
        };
        // One whose `Watching` found no thread to take it off is gone with its `Weak` alone.
        watches.retain(|held| held.strong_count() > 0 && !held.ptr_eq(&Arc::downgrade(watch)));
        let none_left = watches.is_empty();
        if none_left {
            names.remove(name);
        }
        none_left
    }

    fn of(&self, name: &str) -> Vec<Arc<Reconciler>> {
        let names = self.0.lock().expect("lock poisoned");
        let watches = names.get(name).into_iter().flatten();
        watches.filter_map(Weak::upgrade).collect()
    }
}

/// The watcher binder of one resolver. The host's session keeps it until the host pays for it.
struct Notices(Arc<Watches>);

impl Interface for Notices {}

impl ITopicAddressWatcher for Notices {
    fn r#onAdvertised(&self, name: &str, _generation: i64) -> BinderResult<()> {
        for watch in self.0.of(name) {
            watch.heard();
        }
        Ok(())
    }
}

/// The connection of the newest generation one watch has reached.
struct Reached {
    generation: i64,
    resolved: Resolved,
    /// A session that ended answers for its generation no more: the publisher may live on.
    session: Arc<SessionGuard>,
}

#[derive(Default)]
struct WatchState {
    /// A notice came since the last lookup began: the name may have more than that one said.
    asked: bool,
    running: bool,
    ended: bool,
    /// The connection of the newest generation reached, for the repeats of it, until the end.
    reached: Option<Reached>,
}

/// When a failed round goes again.
struct Retry {
    at: Option<Instant>,
    delay: Duration,
}

impl Default for Retry {
    fn default() -> Self {
        Self {
            at: None,
            delay: FIRST_RETRY,
        }
    }
}

/// Whether another connect to the same socket can answer otherwise (see [`UdsResolver`]).
fn may_connect_again(status: &Status) -> bool {
    let refused = StatusCode::Errno(-Errno::CONNREFUSED.raw_os_error());
    status.exception_code() != ExceptionCode::Security && status.transaction_error() != refused
}

/// One watch: rounds of asking the host and connecting, on a notice or after one that failed.
pub(super) struct Reconciler {
    name: String,
    /// Weak: the host's session keeps its notices' binder, which reaches this.
    host: Weak<Host>,
    options: Options,
    on_resolved: OnResolved,
    state: Mutex<WatchState>,
    /// Wakes a thread waiting to go round again.
    changed: Condvar,
    /// Held across a connect and its call, so lookups and notices keep the generations' order.
    order: Mutex<()>,
}

impl Reconciler {
    fn new(name: &str, host: Weak<Host>, options: Options, on_resolved: OnResolved) -> Self {
        Self {
            name: name.to_owned(),
            host,
            options,
            on_resolved,
            state: Mutex::default(),
            changed: Condvar::new(),
            order: Mutex::default(),
        }
    }

    /// The name changed, or may have: ask again. Takes no lock a round holds across a call.
    pub(super) fn heard(self: &Arc<Self>) {
        let mut state = self.state.lock().expect("lock poisoned");
        if state.ended {
            return;
        }
        state.asked = true;
        self.changed.notify_all();
        if state.running {
            return;
        }
        state.running = true;
        drop(state);
        let reconciler = self.clone();
        let spawned = std::thread::Builder::new()
            .name("rsbinder-topic-dial".into())
            .spawn(move || reconciler.run());
        if let Err(error) = spawned {
            log::warn!("topic: no thread to ask for {:?}: {error}", self.name);
            self.state.lock().expect("lock poisoned").running = false;
        }
    }

    /// Whether to go round again; a no and the thread's end are one hold of the lock `heard` takes.
    fn go_round(&self, retry: &mut Retry) -> bool {
        let mut state = self.state.lock().expect("lock poisoned");
        loop {
            if state.ended {
                state.running = false;
                return false;
            }
            if std::mem::take(&mut state.asked) {
                *retry = Retry::default();
                return true;
            }
            let Some(at) = retry.at else {
                state.running = false;
                return false;
            };
            let Some(wait) = at.checked_duration_since(Instant::now()) else {
                retry.at = None;
                return true;
            };
            state = self
                .changed
                .wait_timeout(state, wait)
                .expect("lock poisoned")
                .0;
        }
    }

    /// `order` guards no data: a callback's panic under it leaves nothing to repair.
    fn order(&self) -> MutexGuard<'_, ()> {
        self.order.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn run(self: Arc<Self>) {
        let mut retry = Retry::default();
        while self.go_round(&mut retry) {
            let round = std::panic::AssertUnwindSafe(|| self.round());
            // A panic ends the round alone; a notice that came meanwhile is `asked`, for the next.
            let settled = std::panic::catch_unwind(round).unwrap_or_else(|payload| {
                let panic = rsbinder::__panic_message(&payload);
                log::error!(
                    "topic: a watch's call of {:?} panicked ({panic})",
                    self.name
                );
                true
            });
            if settled {
                retry = Retry::default();
            } else {
                retry.at = Some(Instant::now() + retry.delay);
                retry.delay = (retry.delay * 2).min(LAST_RETRY);
            }
        }
    }

    /// Whether a lookup may go: a reply's fd with no slot for it ends the session, and one dials.
    fn descriptors_free(&self) -> bool {
        match std::os::unix::net::UnixStream::pair() {
            Ok(_) => true,
            Err(error) => {
                log::debug!("topic: no descriptors to ask for {:?}: {error}", self.name);
                false
            }
        }
    }

    /// Ask the host and connect; whether the name needs no other round until another notice.
    fn round(&self) -> bool {
        if !self.descriptors_free() {
            return false;
        }
        let Some(host) = self.host.upgrade() else {
            return true;
        };
        let reply = host.registry.r#lookup(&self.name);
        drop(host);
        let (address, generation) = match reply {
            Ok(reply) => parts(reply),
            Err(status) => {
                log::debug!("topic: asking for {:?}: {status}", self.name);
                return status.transaction_error() == StatusCode::DeadObject;
            }
        };
        let Some(address) = address else {
            return true;
        };
        let _order = self.order();
        match self.reach(&address, generation) {
            Ok(Some(resolved)) => {
                if !self.state.lock().expect("lock poisoned").ended {
                    (self.on_resolved)(Ok(resolved));
                }
                true
            }
            Ok(None) => true,
            Err(status) => {
                log::debug!("topic: cannot reach the name's publisher: {status}");
                !may_connect_again(&status)
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

    /// The watch's lookup, whose caller gives up on a failure: the thread goes on.
    fn lookup(self: &Arc<Self>) -> BinderResult<Option<Resolved>> {
        if !self.descriptors_free() {
            self.heard();
            return Ok(None);
        }
        let Some(host) = self.host.upgrade() else {
            return Ok(None);
        };
        let (address, generation) = parts(host.registry.r#lookup(&self.name)?);
        drop(host);
        let Some(address) = address else {
            return Ok(None);
        };
        let reached = self.dial(&address, generation);
        if reached.as_ref().is_err_and(may_connect_again) {
            self.heard();
        }
        reached
    }

    /// Under `order`; the state's lock only between the steps, as [`Self::end`] waits for it.
    fn reach(&self, address: &OwnedFd, generation: i64) -> BinderResult<Option<Resolved>> {
        {
            let state = self.state.lock().expect("lock poisoned");
            if state.ended {
                return Ok(None);
            }
            match state.reached.as_ref() {
                Some(reached) if reached.generation > generation => return Ok(None),
                // Its refusal holds for the generation: a new binder would only hide that.
                Some(reached)
                    if reached.generation == generation && reached.session.was_refused() =>
                {
                    return Ok(None);
                }
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
        let mut state = self.state.lock().expect("lock poisoned");
        if state.ended {
            drop(state);
            return Ok(None);
        }
        let replaced = state.reached.replace(Reached {
            generation,
            resolved: resolved.clone(),
            session,
        });
        drop(state);
        drop(replaced);
        Ok(Some(resolved))
    }

    /// Tell the watch of a publisher its check refused, which no connect to that socket changes.
    fn tell_if_refused(&self, status: &Status) {
        if status.exception_code() != ExceptionCode::Security {
            return;
        }
        log::warn!("topic: the name's publisher is refused: {status}");
        if !self.state.lock().expect("lock poisoned").ended {
            (self.on_resolved)(Err(status.clone()));
        }
    }

    /// Stop connecting, and let go of the connection, which a call in it may be held in for good.
    pub(super) fn end(&self) {
        let mut state = self.state.lock().expect("lock poisoned");
        state.ended = true;
        let reached = state.reached.take();
        self.changed.notify_all();
        drop(state);
        drop(reached);
    }

    /// No thread of this watch runs and none is to, for a test.
    #[cfg(test)]
    pub(super) fn idle(&self) -> bool {
        let state = self.state.lock().expect("lock poisoned");
        !state.running && !state.asked
    }
}
