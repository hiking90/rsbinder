// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 2-24 §5.2: a TCP link that goes silent, cut for real.
//!
//! The hermetic tests check that a session arms keepalive,
//! `TCP_USER_TIMEOUT` and `SO_SNDTIMEO` with the right values
//! (`getsockopt`), and that an `ETIMEDOUT` a fake transport raises ends the
//! session. Neither shows the kernel acting on those options — giving up on
//! a peer whose packets stop arriving — nor the `ETIMEDOUT` it then raises
//! travelling this crate's path to `binder_died`. That takes packets that
//! vanish without a FIN or a reset, which only the kernel's packet filter
//! produces.
//!
//! `link_break_in_a_network_namespace` runs this binary again under
//! `unshare -rn`: a user namespace and a network namespace of its own, in
//! which an unprivileged process holds `CAP_NET_ADMIN` over that network
//! namespace alone. There it brings `lo` up, adds an `nft` output chain and
//! runs the `in_ns_*` tests, each serving on a port of its own over loopback
//! and dropping that port's packets both ways once its session is up. A host
//! that refuses unprivileged user namespaces (Ubuntu 24.04's AppArmor
//! default) or has no `unshare` or `nft` skips, saying so; run the outer
//! test as root there. With `RSB_LINK_BREAK_REQUIRED` set (CI) it fails
//! instead of skipping.
//!
//! Every case with a timeout uses 3 s: keepalive's first probe after 1 s of
//! silence and three more 1 s apart (whole seconds, at least one), and
//! `TCP_USER_TIMEOUT` = 3 s (`RpcTransport::set_liveness`).

#![cfg(all(target_os = "linux", feature = "rpc-tcp-debug", feature = "rpc-tls"))]

use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rsbinder::rpc::rustls::pki_types::pem::PemObject;
use rsbinder::rpc::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rsbinder::rpc::rustls::{ClientConfig, RootCertStore, ServerConfig};
use rsbinder::rpc::transport::TcpDebugTransport;
use rsbinder::rpc::{RpcClientConfig, RpcProxy, RpcServer, RpcSession};
use rsbinder::{
    Binder, DeathRecipient, Interface, Parcel, Remotable, Result, SIBinder, Status, StatusCode,
    TransactionCode, WIBinder, FIRST_CALL_TRANSACTION, FLAG_ONEWAY,
};

/// Set for the run inside the namespace; the `in_ns_*` tests do nothing without it.
const IN_NS: &str = "RSB_LINK_BREAK_IN_NS";
/// Set where a skip would hide a gap (CI): a host that cannot run the cases fails the test.
const REQUIRED: &str = "RSB_LINK_BREAK_REQUIRED";
/// The `in_ns_*` tests below, so the outer test can tell that every one ran.
const CASES: usize = 10;

const TIMEOUT: Duration = Duration::from_secs(3);
/// The first probe after 1 s and three 1 s apart end it near 4 s; margin for a loaded host.
const NOTICED_WITHIN: Duration = Duration::from_secs(8);
/// A peer that stops acknowledging ends nothing at once: no FIN or reset arrives.
const NOT_BEFORE: Duration = Duration::from_millis(900);

const DESC: &str = "rsbinder.test.ILinkBreak";
const TX_ECHO: TransactionCode = FIRST_CALL_TRANSACTION;
const TX_SLOW: TransactionCode = FIRST_CALL_TRANSACTION + 1;

const CA: &str = include_str!("tls_fixtures/ca.crt");
const SRV_CRT: &str = include_str!("tls_fixtures/srv.crt");
const SRV_KEY: &str = include_str!("tls_fixtures/srv.key");

#[test]
fn link_break_in_a_network_namespace() {
    let refused = |what: &str| {
        assert!(
            std::env::var_os(REQUIRED).is_none(),
            "{REQUIRED} is set: {what}"
        );
        eprintln!("SKIP link_break_in_a_network_namespace: {what}");
    };
    match Command::new("unshare").args(["-rn", "true"]).status() {
        Err(e) => return refused(&format!("cannot run `unshare`: {e}")),
        Ok(s) if !s.success() => {
            return refused("`unshare -rn` is refused (unprivileged user namespaces off?)")
        }
        Ok(_) => {}
    }
    if let Err(e) = Command::new("nft").arg("--version").output() {
        return refused(&format!("cannot run `nft`: {e}"));
    }
    let exe = std::env::current_exe().expect("current_exe");
    let out = std::env::temp_dir().join(format!("rsb_link_break_{}.out", std::process::id()));
    let log = std::fs::File::create(&out).expect("create the child's output file");
    let setup = "ip link set lo up \
        && nft add table inet rsb \
        && nft add chain inet rsb out '{ type filter hook output priority 0; }' \
        && exec \"$0\" --ignored --test-threads=16 in_ns_";
    let mut child = Command::new("unshare")
        .args(["-rn", "sh", "-c", setup])
        .arg(&exe)
        .env(IN_NS, "1")
        .stdout(Stdio::from(log))
        .spawn()
        .expect("spawn the run in a namespace");
    // Bounded: a case that hangs would otherwise hang the suite and report nothing.
    let deadline = Instant::now() + Duration::from_secs(120);
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("the run in the namespace was still going after 120 s");
        }
        thread::sleep(Duration::from_millis(100));
    };
    let printed = std::fs::read_to_string(&out).unwrap_or_default();
    let _ = std::fs::remove_file(&out);
    println!("{printed}");
    assert!(status.success(), "a case failed in the namespace: {status}");
    assert!(
        printed.contains(&format!("test result: ok. {CASES} passed")),
        "every in-namespace case ran"
    );
}

fn in_ns() -> bool {
    let inside = std::env::var_os(IN_NS).is_some();
    if !inside {
        eprintln!("run by link_break_in_a_network_namespace, inside a network namespace");
    }
    inside
}

/// Drop `port`'s packets both ways from now on; the sockets stay open.
fn cut(port: u16) {
    for dir in ["dport", "sport"] {
        let rule = format!("add rule inet rsb out tcp {dir} {port} drop");
        let ok = Command::new("nft")
            .args(rule.split(' '))
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "nft {rule}");
    }
}

struct Svc;
impl Interface for Svc {}
impl Remotable for Svc {
    fn descriptor() -> &'static str {
        DESC
    }
    fn on_transact(&self, code: TransactionCode, r: &mut Parcel, reply: &mut Parcel) -> Result<()> {
        match code {
            TX_ECHO => {
                let payload: Vec<u8> = r.read()?;
                reply.write(&Status::from(StatusCode::Ok))?;
                reply.write(&(payload.len() as i32))
            }
            TX_SLOW => {
                let ms: i32 = r.read()?;
                thread::sleep(Duration::from_millis(ms.max(0) as u64));
                reply.write(&Status::from(StatusCode::Ok))
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }
    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}

struct Died(Arc<Mutex<Option<Instant>>>);
impl DeathRecipient for Died {
    fn binder_died(&self, _who: &WIBinder) {
        self.0.lock().unwrap().get_or_insert(Instant::now());
    }
}

#[derive(Clone, Copy)]
enum Link {
    Tcp,
    Tls,
}

fn certs(pem: &str) -> Vec<CertificateDer<'static>> {
    CertificateDer::pem_slice_iter(pem.as_bytes())
        .collect::<std::result::Result<_, _>>()
        .expect("parse certs")
}

fn server_config() -> Arc<ServerConfig> {
    let key = PrivateKeyDer::from_pem_slice(SRV_KEY.as_bytes()).expect("parse key");
    Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs(SRV_CRT), key)
            .expect("server config"),
    )
}

fn client_config() -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    for c in certs(CA) {
        roots.add(c).expect("add ca");
    }
    Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

/// A session over `link` to a server on `port`, with an incoming connection and a death link.
struct Fixture {
    session: RpcSession,
    root: SIBinder,
    died: Arc<Mutex<Option<Instant>>>,
    _recipient: Arc<dyn DeathRecipient>,
    _server: Arc<RpcServer>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.session.close_session();
    }
}

impl Fixture {
    fn new(link: Link, port: u16, timeout: Option<Duration>) -> Fixture {
        let root_obj = Binder::new(Svc).as_binder();
        let server = match link {
            Link::Tcp => {
                // `serve_connection` needs a server object, whose own listener is never run.
                let path = std::env::temp_dir()
                    .join(format!("rsb_link_{port}_{}.sock", std::process::id()));
                let _ = std::fs::remove_file(&path);
                let server = RpcServer::setup_unix_server(&path).expect("server");
                server.set_android13plus(2);
                server.set_root(root_obj).expect("set_root");
                let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind");
                let accepting = server.clone();
                thread::spawn(move || {
                    for stream in listener.incoming() {
                        let Ok(stream) = stream else { return };
                        let Ok(t) = TcpDebugTransport::from_stream(stream) else {
                            return;
                        };
                        accepting.serve_connection(Box::new(t));
                    }
                });
                server
            }
            Link::Tls => {
                let server = RpcServer::setup_tcp_server_tls(("127.0.0.1", port), server_config())
                    .expect("server");
                server.set_android13plus(2);
                server.set_root(root_obj).expect("set_root");
                let _ = server.run_background();
                server
            }
        };
        let config = match link {
            Link::Tcp => RpcClientConfig::tcp_debug(([127, 0, 0, 1], port).into(), 2),
            Link::Tls => RpcClientConfig::tls("127.0.0.1", port, "localhost", client_config(), 2),
        };
        let mut config = config.incoming_connections(1);
        if let Some(timeout) = timeout {
            config = config.timeout(timeout);
        }
        let session = RpcSession::setup_client_android13plus_with_config(config).expect("connect");
        let root = session.get_root().expect("root");
        let died = Arc::new(Mutex::new(None));
        let recipient: Arc<dyn DeathRecipient> = Arc::new(Died(died.clone()));
        root.link_to_death(Arc::downgrade(&recipient))
            .expect("link_to_death");
        let f = Fixture {
            session,
            root,
            died,
            _recipient: recipient,
            _server: server,
        };
        assert_eq!(f.echo(&[7; 16], 0), Ok(()), "a healthy session answers");
        f
    }

    fn proxy(&self) -> &RpcProxy {
        (*self.root)
            .as_any()
            .downcast_ref::<RpcProxy>()
            .expect("RpcProxy")
    }

    fn echo(&self, payload: &[u8], flags: u32) -> Result<()> {
        let mut d = self.proxy().build_request(DESC)?;
        d.write(&payload.to_vec())?;
        self.proxy().transact(TX_ECHO, &d, flags).map(|_| ())
    }

    fn slow(&self, ms: i32) -> Result<()> {
        let mut d = self.proxy().build_request(DESC)?;
        d.write(&ms)?;
        self.proxy().transact(TX_SLOW, &d, 0).map(|_| ())
    }

    /// When the death recipient fired, waiting up to `wait` for it.
    fn died_within(&self, wait: Duration) -> Option<Instant> {
        let deadline = Instant::now() + wait;
        loop {
            if let Some(at) = *self.died.lock().unwrap() {
                return Some(at);
            }
            if Instant::now() > deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

/// The control: a quiet session whose peer's host answers is never ended by the probes.
fn healthy_idle_session_survives(link: Link, port: u16) {
    let f = Fixture::new(link, port, Some(TIMEOUT));
    // Twice the probes' four seconds: every probe round has come and been answered.
    assert_eq!(
        f.died_within(NOTICED_WITHIN),
        None,
        "a live link was declared dead"
    );
    assert_eq!(f.echo(&[1; 16], 0), Ok(()));
}

/// Keepalive and `TCP_USER_TIMEOUT` end a session nobody is using once its link goes silent.
fn idle_session_ends_when_the_link_goes_silent(link: Link, port: u16) {
    let f = Fixture::new(link, port, Some(TIMEOUT));
    cut(port);
    let cut_at = Instant::now();
    let died = f
        .died_within(NOTICED_WITHIN)
        .expect("the silent link never ended the session");
    let after = died - cut_at;
    assert!(
        after >= NOT_BEFORE,
        "ended after {after:?}: not by the probes"
    );
    assert_eq!(f.echo(&[1; 16], 0), Err(StatusCode::DeadObject));
}

/// A reply that cannot come ends the session within the timeout.
fn reply_wait_ends_when_the_link_goes_silent(link: Link, port: u16) {
    let f = Arc::new(Fixture::new(link, port, Some(TIMEOUT)));
    let (tx, rx) = mpsc::channel();
    let waiting = f.clone();
    thread::spawn(move || {
        let got = waiting.slow(30_000);
        let _ = tx.send((got, Instant::now()));
    });
    // Let the request go out before the cut, so it is the reply that is lost.
    thread::sleep(Duration::from_millis(300));
    cut(port);
    let cut_at = Instant::now();
    let (got, at) = rx
        .recv_timeout(NOTICED_WITHIN)
        .expect("the reply wait outlived the silent link");
    assert!(got.is_err(), "a reply crossed a cut link: {got:?}");
    assert!(at - cut_at <= NOTICED_WITHIN);
    assert!(
        f.died_within(Duration::from_secs(1)).is_some(),
        "the session outlived the call"
    );
}

/// A send the peer never acknowledges fails within the timeout and ends the session.
fn send_ends_when_the_link_goes_silent(link: Link, port: u16) {
    let f = Fixture::new(link, port, Some(TIMEOUT));
    cut(port);
    let cut_at = Instant::now();
    // Oneway, so each returns once written; the socket buffers fill, then the send stalls.
    let payload = vec![0u8; 256 * 1024];
    let failed = loop {
        if let Err(e) = f.echo(&payload, FLAG_ONEWAY) {
            break e;
        }
        assert!(
            cut_at.elapsed() <= NOTICED_WITHIN,
            "sends kept succeeding over a cut link"
        );
    };
    let after = cut_at.elapsed();
    assert!(
        after >= NOT_BEFORE,
        "failed after {after:?} ({failed:?}): not by a deadline"
    );
    assert!(after <= NOTICED_WITHIN, "failed only after {after:?}");
    assert!(
        f.died_within(Duration::from_secs(1)).is_some(),
        "the session outlived the send"
    );
}

/// The other control: without a timeout the system's keepalive (hours) is all there is.
fn without_a_timeout_the_session_waits(link: Link, port: u16) {
    let f = Fixture::new(link, port, None);
    cut(port);
    assert_eq!(
        f.died_within(NOTICED_WITHIN),
        None,
        "something other than the timeout's probes ended it"
    );
}

macro_rules! cases {
    ($($name:ident => $case:ident($link:expr, $port:expr);)*) => {$(
        #[test]
        #[ignore = "run inside a network namespace by link_break_in_a_network_namespace"]
        fn $name() {
            if in_ns() {
                $case($link, $port);
            }
        }
    )*};
}

cases! {
    in_ns_tcp_healthy_idle_session_survives => healthy_idle_session_survives(Link::Tcp, 7101);
    in_ns_tls_healthy_idle_session_survives => healthy_idle_session_survives(Link::Tls, 7102);
    in_ns_tcp_idle_session_ends => idle_session_ends_when_the_link_goes_silent(Link::Tcp, 7201);
    in_ns_tls_idle_session_ends => idle_session_ends_when_the_link_goes_silent(Link::Tls, 7202);
    in_ns_tcp_reply_wait_ends => reply_wait_ends_when_the_link_goes_silent(Link::Tcp, 7301);
    in_ns_tls_reply_wait_ends => reply_wait_ends_when_the_link_goes_silent(Link::Tls, 7302);
    in_ns_tcp_send_ends => send_ends_when_the_link_goes_silent(Link::Tcp, 7401);
    in_ns_tls_send_ends => send_ends_when_the_link_goes_silent(Link::Tls, 7402);
    in_ns_tcp_without_a_timeout_waits => without_a_timeout_the_session_waits(Link::Tcp, 7501);
    in_ns_tls_without_a_timeout_waits => without_a_timeout_the_session_waits(Link::Tls, 7502);
}
