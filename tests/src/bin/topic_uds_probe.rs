// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The topic-over-a-socket probe of `tests/container/ac_topic_uds.sh` (plan 15-pubsub-uds §7).
//!
//! Prints what each step got, so the script, which runs every process under a uid of its own,
//! judges real credentials:
//!
//! ```text
//! topic_uds_probe OP...
//!   publish:PATH:VALUE       a Publisher at a socket at PATH, mode 0666, publishing VALUE
//!   subscribe:PATH[:UID]     uds::subscribe_with, with `expect_uid` when UID is given
//!   recv                     a value of the last subscriber
//!   advertise:HUB:NAME:VALUE a Publisher of NAME on the hub at socket HUB, publishing VALUE
//!   readvertise:NAME:VALUE   the same on the session of the last `advertise`
//!   flood:HUB:COUNT          sessions kept with the hub until one fails: `RESULT flood <n> OK`
//!   named:HUB:NAME[:UID]     a NamedSubscriber of NAME, publishers checked against UID if given
//!   nrecv                    a value of the last NamedSubscriber
//!   list:HUB:PREFIX          the hub's names, a `LISTED <name> <uid>` line for each
//!   lookup:HUB:NAME          the hub's `lookup`: FOUND or NULL, the descriptor kept for `connect`
//!   peer                     who listens at the kept descriptor, `PEER <uid> <pid>`
//!   connect                  a plain connect through the kept descriptor
//!   say:TEXT  wait:FILE  sleep:MS  hold[:SECS]
//! ```
//!
//! A step prints `RESULT <op> <arg> <outcome>`, the outcome `OK`, or how it was refused:
//! `SECURITY`, `EACCES`, `TAKEN`, `ECONNREFUSED`, `ERR:<status>`.

#[cfg(any(target_os = "linux", target_os = "android"))]
mod probe {
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;
    use std::time::Duration;

    use rsbinder::rpc::{PeerIdentity, RpcClientConfig, RpcServer, RpcServerHandle, RpcSession};
    use rsbinder::{ExceptionCode, Status, StatusCode};
    use rsbinder_topic::aidl::ITopicUdsRegistry::{ITopicUdsRegistry, ERROR_TAKEN};
    use rsbinder_topic::registry::uds::{ResolverOptions, UdsResolver};
    use rsbinder_topic::uds::{self, Endpoint, Options};
    use rsbinder_topic::{
        Bytes, NamedSubscriber, Publisher, PublisherPolicy, Start, Subscriber, SubscriberPolicy,
    };

    fn outcome<T>(result: &Result<T, Status>) -> String {
        match result {
            Ok(_) => "OK".to_owned(),
            Err(status) if status.exception_code() == ExceptionCode::Security => {
                "SECURITY".to_owned()
            }
            Err(status)
                if status.exception_code() == ExceptionCode::ServiceSpecific
                    && status.service_specific_error() == ERROR_TAKEN =>
            {
                "TAKEN".to_owned()
            }
            Err(status) if status.transaction_error() == StatusCode::Errno(-libc::EACCES) => {
                "EACCES".to_owned()
            }
            Err(status) => format!("ERR:{status}"),
        }
    }

    fn result(op: &str, arg: &str, outcome: &str) {
        println!("RESULT {op} {arg} {outcome}");
    }

    /// What the steps made, kept until the process ends.
    #[derive(Default)]
    struct Held {
        publishers: Vec<(Publisher<Bytes>, Option<RpcServerHandle>)>,
        subscribers: Vec<Subscriber<Bytes>>,
        named: Vec<NamedSubscriber<Bytes>>,
        resolvers: Vec<UdsResolver>,
        address: Option<OwnedFd>,
    }

    fn policy() -> SubscriberPolicy {
        SubscriberPolicy {
            start: Start::Last,
            ..SubscriberPolicy::default()
        }
    }

    fn publish(held: &Mutex<Held>, path: &str, value: &str) {
        let server = RpcServer::setup_unix_server(path).expect("a socket");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666)).expect("a mode");
        let publisher = Publisher::<Bytes>::new(&PublisherPolicy::default()).expect("a publisher");
        let handle = uds::serve(&server, publisher.as_binder()).expect("a server");
        publisher
            .publish(&Bytes(value.as_bytes().to_vec()))
            .expect("a record");
        let mut held = held.lock().unwrap();
        held.publishers.push((publisher, Some(handle)));
        result("publish", path, "OK");
    }

    fn subscribe(held: &Mutex<Held>, path: &str, expect: Option<&str>) {
        let options = Options {
            expect_uid: expect.map(|uid| uid.parse().expect("a uid")),
            ..Options::default()
        };
        let endpoint = Endpoint::Path(path.into());
        let subscriber = uds::subscribe_with::<Bytes>(endpoint, &policy(), &options);
        result("subscribe", path, &outcome(&subscriber));
        if let Ok(subscriber) = subscriber {
            held.lock().unwrap().subscribers.push(subscriber);
        }
    }

    fn recv(held: &Mutex<Held>) {
        let mut held = held.lock().unwrap();
        let subscriber = held.subscribers.last_mut().expect("a subscriber");
        match subscriber.recv_timeout(Duration::from_secs(10)) {
            Ok(Bytes(value)) => result("recv", &String::from_utf8_lossy(&value), "OK"),
            Err(why) => result("recv", "-", &format!("ERR:{why}")),
        }
    }

    fn resolver(hub: &str, expect: Option<&str>) -> Result<UdsResolver, Status> {
        let options = ResolverOptions {
            expect_publisher_uid: expect.map(|uid| uid.parse().expect("a uid")),
            ..ResolverOptions::default()
        };
        UdsResolver::connect(Endpoint::Path(hub.into()), &options)
    }

    fn advertise(held: &Mutex<Held>, hub: &str, name: &str, value: &str) {
        let made = resolver(hub, None).and_then(|resolver| {
            let publisher =
                Publisher::<Bytes>::advertise_with(name, &PublisherPolicy::default(), &resolver)?;
            Ok((resolver, publisher))
        });
        result("advertise", name, &outcome(&made));
        if let Ok((resolver, publisher)) = made {
            publisher
                .publish(&Bytes(value.as_bytes().to_vec()))
                .expect("a record");
            let mut held = held.lock().unwrap();
            held.resolvers.push(resolver);
            held.publishers.push((publisher, None));
        }
    }

    fn readvertise(held: &Mutex<Held>, name: &str, value: &str) {
        let mut held = held.lock().unwrap();
        let resolver = held.resolvers.last().expect("a resolver").clone();
        let made = Publisher::<Bytes>::advertise_with(name, &PublisherPolicy::default(), &resolver);
        result("advertise", name, &outcome(&made));
        if let Ok(publisher) = made {
            publisher
                .publish(&Bytes(value.as_bytes().to_vec()))
                .expect("a record");
            held.publishers.push((publisher, None));
        }
    }

    /// A short deadline: a connection past the hub's bound waits in its backlog for this long.
    fn flood(held: &Mutex<Held>, hub: &str, count: usize) {
        let options = ResolverOptions {
            timeout: Some(Duration::from_secs(1)),
            ..ResolverOptions::default()
        };
        let mut made = 0;
        while made < count {
            let endpoint = Endpoint::Path(hub.into());
            let Ok(resolver) = UdsResolver::connect(endpoint, &options) else {
                break;
            };
            held.lock().unwrap().resolvers.push(resolver);
            made += 1;
        }
        result("flood", &made.to_string(), "OK");
    }

    fn named(held: &Mutex<Held>, hub: &str, name: &str, expect: Option<&str>) {
        let made = resolver(hub, expect).and_then(|resolver| {
            let subscriber = NamedSubscriber::<Bytes>::subscribe_with(name, &policy(), &resolver)?;
            Ok((resolver, subscriber))
        });
        result("named", name, &outcome(&made));
        if let Ok((resolver, subscriber)) = made {
            let mut held = held.lock().unwrap();
            held.resolvers.push(resolver);
            held.named.push(subscriber);
        }
    }

    fn nrecv(held: &Mutex<Held>) {
        let mut held = held.lock().unwrap();
        let subscriber = held.named.last_mut().expect("a subscriber");
        match subscriber.recv_timeout(Duration::from_secs(5)) {
            Ok(Bytes(value)) => result("nrecv", &String::from_utf8_lossy(&value), "OK"),
            Err(why) => result("nrecv", "-", &format!("ERR:{why}")),
        }
    }

    fn list(hub: &str, prefix: &str) {
        match resolver(hub, None).and_then(|resolver| resolver.list(prefix)) {
            Ok(listed) => {
                for topic in &listed {
                    println!("LISTED {} {}", topic.name, topic.uid);
                }
                result("list", prefix, "OK");
            }
            Err(status) => result("list", prefix, &outcome::<()>(&Err(status))),
        }
    }

    /// `ITopicUdsRegistry.lookup` by hand: the resolver connects at once, this keeps the fd.
    fn lookup(held: &Mutex<Held>, hub: &str, name: &str) {
        let config = RpcClientConfig::unix(hub, uds_wire()).fd_mode_unix();
        let session = RpcSession::setup_client_android13plus_with_config(config).expect("the hub");
        let registry: rsbinder::Strong<dyn ITopicUdsRegistry> = session
            .get_root()
            .expect("a root")
            .into_interface()
            .expect("a registry");
        match registry.r#lookup(name).map(|found| found.socket) {
            Ok(Some(socket)) => {
                held.lock().unwrap().address = Some(OwnedFd::from(socket));
                result("lookup", name, "FOUND");
            }
            Ok(None) => result("lookup", name, "NULL"),
            Err(status) => result("lookup", name, &outcome::<()>(&Err(status))),
        }
        session.close_session();
    }

    fn uds_wire() -> u32 {
        rsbinder::entry::WireVersion::MAX.get()
    }

    trait FdModeUnix {
        fn fd_mode_unix(self) -> Self;
    }

    impl FdModeUnix for RpcClientConfig {
        fn fd_mode_unix(self) -> Self {
            self.fd_mode(rsbinder::rpc::FileDescriptorTransportMode::Unix)
        }
    }

    fn kept(held: &Mutex<Held>) -> OwnedFd {
        let held = held.lock().unwrap();
        let address = held.address.as_ref().expect("a descriptor from `lookup`");
        address.try_clone().expect("a duplicate")
    }

    fn peer(held: &Mutex<Held>) {
        let config = RpcClientConfig::unix_fd(kept(held), uds_wire()).fd_mode_unix();
        let session =
            RpcSession::setup_client_android13plus_with_config(config).expect("a session");
        match session.peer_identity() {
            PeerIdentity::Local { uid, pid, .. } => println!("PEER {uid} {pid}"),
            other => println!("PEER {other:?}"),
        }
        session.close_session();
    }

    fn connect(held: &Mutex<Held>) {
        let address = kept(held);
        let path = format!("/proc/self/fd/{}", address.as_raw_fd());
        let outcome = match std::os::unix::net::UnixStream::connect(path) {
            Ok(_) => "OK".to_owned(),
            Err(error) if error.raw_os_error() == Some(libc::ECONNREFUSED) => {
                "ECONNREFUSED".to_owned()
            }
            Err(error) => format!("ERR:{error}"),
        };
        result("connect", "-", &outcome);
    }

    pub(super) fn main() {
        env_logger::init();
        let held = Mutex::new(Held::default());
        for op in std::env::args().skip(1) {
            let mut parts = op.split(':');
            let name = parts.next().unwrap_or_default();
            let rest: Vec<&str> = parts.collect();
            match (name, rest.as_slice()) {
                ("publish", [path, value]) => publish(&held, path, value),
                ("subscribe", [path]) => subscribe(&held, path, None),
                ("subscribe", [path, uid]) => subscribe(&held, path, Some(uid)),
                ("recv", []) => recv(&held),
                ("advertise", [hub, name, value]) => advertise(&held, hub, name, value),
                ("readvertise", [name, value]) => readvertise(&held, name, value),
                ("flood", [hub, count]) => flood(&held, hub, count.parse().expect("a count")),
                ("named", [hub, name]) => named(&held, hub, name, None),
                ("named", [hub, name, uid]) => named(&held, hub, name, Some(uid)),
                ("nrecv", []) => nrecv(&held),
                ("list", [hub, prefix]) => list(hub, prefix),
                ("lookup", [hub, name]) => lookup(&held, hub, name),
                ("peer", []) => peer(&held),
                ("connect", []) => connect(&held),
                ("say", [text]) => println!("{text}"),
                ("sleep", [ms]) => {
                    std::thread::sleep(Duration::from_millis(ms.parse().expect("a time")))
                }
                ("wait", [file]) => {
                    while !std::path::Path::new(file).exists() {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
                ("hold", []) => std::thread::sleep(Duration::from_secs(3600)),
                ("hold", [secs]) => {
                    std::thread::sleep(Duration::from_secs(secs.parse().expect("a time")))
                }
                _ => panic!("unknown op {op}"),
            }
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn main() {
    probe::main();
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn main() {
    eprintln!("topic_uds_probe: Linux and Android only");
    std::process::exit(2);
}
