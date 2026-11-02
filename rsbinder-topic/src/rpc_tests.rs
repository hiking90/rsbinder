// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A topic's binders over an RPC session within one process (plans/15-pubsub-uds §4.2): an RPC
//! proxy is one in the same process too, so these need no binder device.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rsbinder::rpc::{FileDescriptorTransportMode, RpcClientConfig, RpcServer, RpcSession};
use rsbinder::{ExceptionCode, Interface, ParcelFileDescriptor, SIBinder, StatusCode, Strong};

use crate::aidl::ITopic::{ITopic, WAKE_FUTEX};
use crate::aidl::ITopicRegistry::ITopicRegistry;
use crate::aidl::ITopicWatcher::BnTopicWatcher;
use crate::aidl::SubscribeRequest::SubscribeRequest;
use crate::connect::Token;
use crate::layout::{ControlPage, MIN_RING, VERSION};
use crate::link::{link_death, Dead};
use crate::pool::MAX_PINS;
use crate::registry::{AllowAll, Registry, RegistryResolver};
use crate::{Bytes, Payload, Publisher, PublisherPolicy};

const WIRE: u32 = 2;
const WAIT: Duration = Duration::from_secs(10);

fn eventually(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < deadline, "{what} within {WAIT:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A server of `root` that passes fds and takes incoming connections, on an abstract socket.
fn serve(root: SIBinder) -> (Vec<u8>, Arc<RpcServer>) {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let serial = NEXT.fetch_add(1, Ordering::Relaxed);
    let name = format!("rsbinder-topic-rpc-{}-{serial}", std::process::id()).into_bytes();
    let server = RpcServer::setup_unix_server_abstract(&name).unwrap();
    server.set_android13plus(WIRE);
    server.set_supported_fd_modes(&[FileDescriptorTransportMode::Unix]);
    server.set_root(root).unwrap();
    std::mem::forget(server.run_background().unwrap());
    (name, server)
}

fn connect(name: &[u8], incoming: u32) -> RpcSession {
    let config = RpcClientConfig::unix_abstract(name, WIRE)
        .fd_mode(FileDescriptorTransportMode::Unix)
        .incoming_connections(incoming);
    RpcSession::setup_client_android13plus_with_config(config).unwrap()
}

/// A request as `Subscriber::request` makes it, with `token` given.
fn request(token: SIBinder) -> (SubscribeRequest, ControlPage) {
    let (fd, page) = ControlPage::create(MAX_PINS).unwrap();
    let request = SubscribeRequest {
        version: VERSION as i32,
        r#type: Bytes::type_name().to_owned(),
        token: Some(token),
        control: Some(ParcelFileDescriptor::from(fd)),
        pins: MAX_PINS as i32,
        wake: WAKE_FUTEX,
        wakeFd: None,
    };
    (request, page)
}

fn publisher() -> Publisher<Bytes> {
    let policy = PublisherPolicy {
        ring_bytes: MIN_RING,
        ..PublisherPolicy::default()
    };
    Publisher::new(&policy).unwrap()
}

#[test]
fn a_death_link_on_an_rpc_binder_fires_when_the_session_ends() {
    let publisher = publisher();
    let (name, server) = serve(publisher.as_binder());
    let session = connect(&name, 1);
    let topic = session.get_root().unwrap();
    assert!(topic.is_remote() && topic.as_proxy().is_none());
    let dead = Arc::new(Dead::default());
    let link = link_death(&topic, Died(dead.clone())).unwrap();
    assert!(link.is_some(), "an RPC binder is another process's");
    assert!(!dead.get());
    server.terminate();
    eventually("the death notice", || dead.get());
    session.close_session();
}

/// The recipient of the test's link.
struct Died(Arc<Dead>);

impl rsbinder::DeathRecipient for Died {
    fn binder_died(&self, who: &rsbinder::WIBinder) {
        self.0.binder_died(who);
    }
}

#[test]
fn a_session_that_would_not_notice_its_end_is_refused_a_death_link() {
    let publisher = publisher();
    let (name, server) = serve(publisher.as_binder());
    let session = connect(&name, 0);
    let topic = session.get_root().unwrap();
    let refused = link_death(&topic, Died(Arc::default())).err();
    assert_eq!(refused, Some(StatusCode::InvalidOperation));
    session.close_session();
    server.terminate();
}

#[test]
fn a_publisher_takes_a_token_from_over_rpc_and_sees_its_session_end() {
    let publisher = publisher();
    let (name, server) = serve(publisher.as_binder());
    let session = connect(&name, 1);
    let topic: Strong<dyn ITopic> = session.get_root().unwrap().into_interface().unwrap();
    let (request, _page) = request(Token::detached());
    topic.r#subscribe(&request).unwrap();
    assert_eq!(publisher.subscriber_count(), 1);
    session.close_session();
    eventually("the subscription's end", || {
        publisher.subscriber_count() == 0
    });
    server.terminate();
}

#[test]
fn a_token_of_the_publishers_own_process_from_over_rpc_is_refused() {
    let publisher = publisher();
    let (name, server) = serve(publisher.as_binder());
    let session = connect(&name, 1);
    let root = session.get_root().unwrap();
    let topic: Strong<dyn ITopic> = root.clone().into_interface().unwrap();
    let (request, _page) = request(root);
    let refused = topic.r#subscribe(&request).err().unwrap();
    assert_eq!(refused.transaction_error(), StatusCode::BadValue);
    assert_eq!(publisher.subscriber_count(), 0);
    session.close_session();
    server.terminate();
}

#[test]
fn a_registry_resolver_refuses_a_binder_of_this_process_and_an_rpc_binder() {
    let registry = Registry::new(AllowAll);
    let own = RegistryResolver::new(registry.as_binder()).err();
    assert_eq!(
        own.map(|status| status.transaction_error()),
        Some(StatusCode::BadValue)
    );
    let (name, server) = serve(registry.as_binder());
    let session = connect(&name, 0);
    let rpc = RegistryResolver::new(session.get_root().unwrap()).err();
    assert_eq!(
        rpc.map(|status| status.transaction_error()),
        Some(StatusCode::InvalidOperation)
    );
    session.close_session();
    server.terminate();
}

/// The kernel registry's death links count kernel handles, so no RPC proxy may enter its table.
#[test]
fn the_kernel_registry_refuses_every_caller_over_rpc() {
    let registry = Registry::new(AllowAll);
    let (name, server) = serve(registry.as_binder());
    let session = connect(&name, 1);
    let held = session.get_root().unwrap();
    let remote: Strong<dyn ITopicRegistry> = held.into_interface().unwrap();

    let publisher = publisher();
    let topic: Strong<dyn ITopic> = publisher.as_binder().into_interface().unwrap();
    let refused = remote.r#advertise("a.name", &topic).err().unwrap();
    assert_eq!(refused.exception_code(), ExceptionCode::Security);

    let watcher = BnTopicWatcher::new_binder(NoNotice);
    let refused = remote.r#watch("a.name", &watcher).err().unwrap();
    assert_eq!(refused.exception_code(), ExceptionCode::Security);

    let refused = remote.r#list("").err().unwrap();
    assert_eq!(refused.exception_code(), ExceptionCode::Security);

    let answer = remote.r#lookup("a.name").unwrap();
    assert!(answer.topic.is_none());
    assert_eq!(answer.generation, i64::MIN);

    remote.r#withdraw("a.name", &topic).unwrap();
    assert_eq!(registry.stats().topics, 0);
    assert_eq!(registry.stats().watchers, 0);
    session.close_session();
    server.terminate();
}

struct NoNotice;

impl Interface for NoNotice {}

impl crate::aidl::ITopicWatcher::ITopicWatcher for NoNotice {
    fn r#onAdvertised(
        &self,
        _: &str,
        _: &Strong<dyn ITopic>,
        _: i64,
    ) -> rsbinder::BinderResult<()> {
        Ok(())
    }
}
