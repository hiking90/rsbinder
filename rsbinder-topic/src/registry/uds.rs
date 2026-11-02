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

use std::fmt;
use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use rsbinder::rpc::{PeerIdentity, RpcServer};
use rsbinder::{
    BinderResult, Caller, DeathLink, DeathRecipient, ExceptionCode, IBinder, Interface,
    ParcelFileDescriptor, SIBinder, Status, StatusCode, Strong, WIBinder,
};
use rustix::fs::{AtFlags, Mode, OFlags};
use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};

use super::resolver::Newest;
use super::table::{Key, Links, Notice, Refused, Sent, Table};
use super::{is_valid_name, link_failed, no_allocation, Action, RegistryAccess, RegistryStats};
use crate::aidl::ITopic::ITopic;
use crate::aidl::ITopicAddressWatcher::{BnTopicAddressWatcher, ITopicAddressWatcher};
use crate::aidl::ITopicUdsRegistry::{
    BnTopicUdsRegistry, ITopicUdsRegistry, ERROR_LIMIT, ERROR_TAKEN,
};
use crate::aidl::TopicAddress::TopicAddress;
use crate::aidl::TopicAllocation::TopicAllocation;
use crate::aidl::TopicInfo::TopicInfo;
use crate::connect::refuse;
use crate::registry::TopicListing;
use crate::resolve::{Advertisement, Resolved, Resolver, Watch};
use crate::uds::{self, Endpoint, Options, SessionGuard};

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
    /// The directory at `path`, made with mode 0700 if it is not there.
    ///
    /// Refused with `InvalidData` when it is a symlink or anything but a directory, when another
    /// uid than root or this process's owns it, or when its group or anyone else can write it:
    /// what another uid made in advance is not used. Whatever a crash left in it is removed.
    pub fn open(path: &Path) -> io::Result<Self> {
        match rustix::fs::mkdir(path, Mode::RWXU) {
            Ok(()) => {}
            Err(rustix::io::Errno::EXIST) => {}
            Err(errno) => return Err(errno.into()),
        }
        let flags = OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::RDONLY | OFlags::CLOEXEC;
        let dir = rustix::fs::open(path, flags, Mode::empty())?;
        let stat = rustix::fs::fstat(&dir)?;
        let refused = |why: String| {
            let path = path.display();
            io::Error::new(io::ErrorKind::InvalidData, format!("{path}: {why}"))
        };
        let own = rustix::process::geteuid().as_raw();
        if stat.st_uid != 0 && stat.st_uid != own {
            return Err(refused(format!(
                "owned by uid {}, neither root nor this process",
                stat.st_uid
            )));
        }
        if stat.st_mode & 0o022 != 0 {
            return Err(refused(format!(
                "group or other can write it (mode {:o})",
                stat.st_mode & 0o7777
            )));
        }
        let this = Self {
            dir,
            next: AtomicU64::new(0),
        };
        this.clear()?;
        Ok(this)
    }

    /// Remove what a crash between a bind and its unlink left.
    fn clear(&self) -> io::Result<()> {
        for entry in rustix::fs::Dir::read_from(&self.dir)? {
            let entry = entry?;
            let name = entry.file_name();
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }
            if let Err(errno) = rustix::fs::unlinkat(&self.dir, name, AtFlags::empty()) {
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

/// The links of a change: `link_to_death` of an RPC proxy sends nothing.
struct ShellLinks<'a> {
    linked: &'a mut std::collections::BTreeMap<u64, Linked>,
    links_made: &'a mut u64,
    this: &'a Weak<Shared>,
}

/// One handle's link. `serial` tells it from an earlier link at the same address, whose notice
/// may still be on its way when an address comes round again.
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
    linked: std::collections::BTreeMap<u64, Linked>,
    links_made: u64,
}

struct Shared {
    access: Box<dyn RegistryAccess>,
    dir: SocketDir,
    cross_uid: std::sync::atomic::AtomicBool,
    next_allocation: AtomicU64,
    /// Taken before `inner` and held while sending: notices leave in the order of their changes.
    fire_order: Mutex<()>,
    inner: Mutex<Inner>,
    this: Weak<Shared>,
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

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().expect("lock poisoned")
    }

    /// `fire_order` guards no data: a panic under it, as a judgement may, leaves nothing to repair.
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

    /// Judge and send the notices of a change, with `fire_order` held. A failed send is the end
    /// of the watcher's session or a send past the server's deadline, which ends it: its watches
    /// go with its death, so there is nothing to send again as a kernel registry does.
    fn deliver(&self, notices: Vec<Notice<TopicKey, WatcherKey>>) {
        for notice in notices {
            if !self
                .access
                .allows(Action::Subscribe, &notice.name, &notice.caller)
            {
                continue;
            }
            let sent = match notice.topic.address.as_deref().map(parcel_address) {
                Some(Ok(address)) => {
                    let generation = notice.generation as i64;
                    let sent = notice
                        .watcher
                        .0
                        .r#onAdvertised(&notice.name, &address, generation);
                    if sent.is_ok() {
                        Sent::Delivered
                    } else {
                        Sent::Failed
                    }
                }
                _ => Sent::Failed,
            };
            if sent == Sent::Failed {
                log::warn!("topic registry: a notice of {:?} failed", notice.name);
            }
            let now = Instant::now();
            self.lock()
                .table
                .sent(&notice.name, &notice.watcher, notice.generation, sent, now);
        }
    }
}

/// The host side of a registry of topic names over a Unix socket: serve
/// [`as_binder`](Self::as_binder) as the root of an [`RpcServer`] set up with [`serve`](Self::serve).
pub struct UdsRegistry {
    shared: Arc<Shared>,
    binder: SIBinder,
}

/// How long a send to a watcher may take: a stopped watcher whose socket is full holds the
/// registry's other changes while it sends, and the session of one that runs out ends, which
/// takes its watches away. It is what `RpcServer::set_handshake_timeout` gives a silent peer.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

impl UdsRegistry {
    /// A registry whose callers `access` judges and whose sockets are made in `dir`.
    pub fn new(dir: SocketDir, access: impl RegistryAccess + 'static) -> Self {
        let shared = Arc::new_cyclic(|this| Shared {
            access: Box::new(access),
            dir,
            cross_uid: std::sync::atomic::AtomicBool::new(false),
            next_allocation: AtomicU64::new(1),
            fire_order: Mutex::new(()),
            inner: Mutex::new(Inner {
                table: Table::new(),
                linked: std::collections::BTreeMap::new(),
                links_made: 0,
            }),
            this: this.clone(),
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
    /// settings a client needs, and a deadline on what it sends, so that a stopped watcher
    /// holds the registry for a bounded time.
    pub fn serve(&self, server: &RpcServer) -> BinderResult<()> {
        uds::configure(server);
        server.set_root(self.as_binder())?;
        server.set_reply_timeout(Some(SEND_TIMEOUT))?;
        Ok(())
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
    /// The bound on each step of connecting and on the call that sets a connection up, to the
    /// registry and to each publisher ([`Options::timeout`]). Default 10 s.
    pub timeout: Option<Duration>,
    /// The uid the registry's host has to be ([`Options::expect_uid`]). Default `None`.
    pub expect_host_uid: Option<u32>,
    /// The uid every publisher a name is given to has to be: a second check beside the host's
    /// policy, for a host that was set up wrong. A publisher of another uid is not subscribed
    /// to. Default `None`.
    pub expect_publisher_uid: Option<u32>,
}

impl Default for ResolverOptions {
    fn default() -> Self {
        Self {
            timeout: Options::default().timeout,
            expect_host_uid: None,
            expect_publisher_uid: None,
        }
    }
}

/// The session with the host, closed once the resolver and all it made have let go of it.
struct Host {
    registry: Strong<dyn ITopicUdsRegistry>,
    _session: SessionGuard,
    /// How a connection to a publisher goes.
    publisher: Options,
}

/// A [`UdsRegistry`] as a [`Resolver`]: names under the policy of its host, with no binder device.
///
/// One value is one session with the host. `advertise` serves the topic on a socket the host
/// makes, and the [`Advertisement`] ends both: its drop withdraws the name and stops accepting,
/// leaving the sessions of subscribers already connected, so that they read the topic's end
/// record; they close as those subscribers end, or with this process. A
/// [`Publisher`](crate::Publisher) holds one advertisement, so it is on one registry at a time,
/// kernel or over a socket; a binder registered with the service manager as well takes none.
///
/// `watch` makes one session for each generation of the name, on a thread of the watch's, so one
/// publisher that is slow to accept holds up only its own name's watch. A connect into a publisher
/// whose accept queue is full waits in `connect(2)` until it accepts or ends: that thread is held
/// that long, and [`ResolverOptions::timeout`] does not reach it.
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
            }),
        })
    }

    /// The names under `prefix` with a topic that this process may subscribe to, sorted, as the
    /// registry sends them. `EX_SECURITY` when the host's policy does not let this process list.
    pub fn list(&self, prefix: &str) -> BinderResult<Vec<TopicListing>> {
        let listed = self.host.registry.r#list(prefix)?;
        Ok(listed.into_iter().map(TopicListing::from).collect())
    }

    fn serve(socket: OwnedFd, topic: SIBinder) -> BinderResult<Arc<RpcServer>> {
        let server = RpcServer::setup_unix_server_from_bound_fd(socket)?;
        uds::configure(&server);
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
        Ok(server)
    }
}

impl fmt::Debug for UdsResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UdsResolver").finish_non_exhaustive()
    }
}

/// Withdraws the name when the advertisement is dropped, and stops taking new subscribers.
struct Advertised {
    host: Arc<Host>,
    server: Arc<RpcServer>,
    name: String,
    topic: Strong<dyn ITopic>,
    allocation: i64,
}

impl Drop for Advertised {
    fn drop(&mut self) {
        let withdrawn = self
            .host
            .registry
            .r#withdraw(&self.name, &self.topic, self.allocation);
        if let Err(status) = withdrawn {
            log::warn!(
                "topic registry: withdrawing {:?} failed: {status}",
                self.name
            );
        }
        self.server.stop_accepting();
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
            .and_then(|socket| Self::serve(OwnedFd::from(socket), topic));
        let server = match served {
            Ok(server) => server,
            Err(status) => {
                if let Err(withdrawn) = self.host.registry.r#withdraw(name, &typed, allocation) {
                    log::warn!("topic registry: withdrawing {name:?} failed: {withdrawn}");
                }
                return Err(status);
            }
        };
        if let Err(status) = self.host.registry.r#commit(name, &typed, allocation) {
            // The host dropped the allocation with its refusal, or with this session's end.
            server.stop_accepting();
            return Err(status);
        }
        Ok(Advertisement::new(Advertised {
            host: self.host.clone(),
            server,
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
        let on_resolved = Box::new(move |resolved: Resolved| on_registered(resolved.into_binder()));
        self.watch_resolved(name, on_resolved)
    }

    fn watch_resolved(
        &self,
        name: &str,
        on_resolved: Box<dyn Fn(Resolved) + Send + Sync>,
    ) -> BinderResult<Watch> {
        let dialer = Arc::new(Dialer {
            options: self.host.publisher.clone(),
            on_resolved,
            queue: Mutex::default(),
            latest: Mutex::default(),
        });
        let dialing = dialer.clone();
        let newest = Arc::new(Newest::new(Box::new(
            move |(address, generation): (OwnedFd, i64)| dialing.give(address, generation),
        )));
        let watcher = BnTopicAddressWatcher::new_binder(Notices {
            name: name.to_owned(),
            newest: newest.clone(),
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
                Some((address, generation)) => asked.dial(address, generation),
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
        let _ = self.host.registry.r#unwatch(&self.name, &self.watcher);
    }
}

/// The watcher binder of one watch.
struct Notices {
    name: String,
    newest: Arc<Newest<(OwnedFd, i64)>>,
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
        let address = socket.as_ref().try_clone().map_err(StatusCode::from)?;
        self.newest.heard(Some((address, generation)), generation);
        Ok(())
    }
}

/// The connection of the newest generation one watch has reached.
struct Reached {
    generation: i64,
    resolved: Resolved,
}

#[derive(Default)]
struct DialQueue {
    next: Option<(OwnedFd, i64)>,
    running: bool,
    ended: bool,
}

/// Turns the descriptors of one watch into connections, on a thread of its own: a notice runs on
/// the session with the host, where a connect that waits would hold every other name's notices.
struct Dialer {
    options: Options,
    on_resolved: Box<dyn Fn(Resolved) + Send + Sync>,
    queue: Mutex<DialQueue>,
    /// Held across a connect: lookups and notices of one generation make one session between
    /// them, and give the same binder, which a subscriber takes as the same publisher.
    latest: Mutex<Option<Reached>>,
}

impl Dialer {
    /// A notice or the watch's reply: connect to the newest of them, the older go unused.
    fn give(self: &Arc<Self>, address: OwnedFd, generation: i64) {
        let mut queue = self.queue.lock().expect("lock poisoned");
        if queue.ended {
            return;
        }
        queue.next = Some((address, generation));
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

    fn run(self: Arc<Self>) {
        loop {
            let mut queue = self.queue.lock().expect("lock poisoned");
            let Some((address, generation)) = queue.next.take() else {
                queue.running = false;
                return;
            };
            drop(queue);
            let known = self.latest.lock().expect("lock poisoned");
            let repeated = known.as_ref().is_some_and(|r| r.generation >= generation);
            drop(known);
            if repeated {
                continue;
            }
            match self.dial(address, generation) {
                Ok(Some(resolved)) => {
                    if !self.queue.lock().expect("lock poisoned").ended {
                        (self.on_resolved)(resolved);
                    }
                }
                Ok(None) => {}
                // The publisher ended or will not be subscribed to: the next generation is asked.
                Err(status) => log::debug!("topic: cannot reach the name's publisher: {status}"),
            }
        }
    }

    /// The connection of `generation`, made here unless it exists; `None` for an older one.
    fn dial(&self, address: OwnedFd, generation: i64) -> BinderResult<Option<Resolved>> {
        let mut latest = self.latest.lock().expect("lock poisoned");
        match latest.as_ref() {
            Some(reached) if reached.generation > generation => return Ok(None),
            Some(reached) if reached.generation == generation => {
                return Ok(Some(reached.resolved.clone()));
            }
            _ => {}
        }
        let session = uds::connect(Endpoint::Fd(address), &self.options)?;
        let binder = session.0.get_root()?;
        session.0.set_reply_timeout(None)?;
        let resolved = Resolved::with_session(binder, Arc::new(session));
        *latest = Some(Reached {
            generation,
            resolved: resolved.clone(),
        });
        Ok(Some(resolved))
    }

    fn end(&self) {
        let mut queue = self.queue.lock().expect("lock poisoned");
        queue.ended = true;
        queue.next = None;
    }
}
