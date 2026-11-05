// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! vsock between a host and a real VM: `tests/scripts/run_vsock_vm.sh` runs these.
//!
//! `tests/rpc_vsock.rs` stays on `VMADDR_CID_LOCAL`, which cannot show the peer's cid, the
//! virtqueue path, or what happens when the VM goes away. Each `#[ignore]`d test here is one side
//! of a scenario, so running it alone only waits for a peer that never comes. Which test runs
//! where, and which side kills the VM, is the script's.
//!
//! Environment, on both sides: `rsbvm_peer_cid` and `rsbvm_port`. `rsbvm_qemu_pidfile` is on the
//! host only: the tests named `guest_killed_*` and a handler of `TX_KILL_GUEST` read it to
//! `SIGKILL` the VM.

#![cfg(all(feature = "rpc-vsock", feature = "rpc-tls", target_os = "linux"))]

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rsbinder::entry::{WireProfile, WireVersion};
use rsbinder::rpc::transport::TlsTransport;
use rsbinder::rpc::{
    AddressSpace, EndReason, PeerIdentity, RpcProxy, RpcServer, RpcSession, RpcTransport,
};
use rsbinder::{
    Binder, Caller, Client, DeathRecipient, Endpoint, Interface, Parcel, Remotable, RemoteProxy,
    Result, SIBinder, Status, StatusCode, TransactionCode, Uri, WIBinder, FIRST_CALL_TRANSACTION,
    FLAG_ONEWAY,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use vsock::VMADDR_CID_ANY;

const DESC: &str = "rsbinder.test.IVsockVm";
const TX_PING: TransactionCode = FIRST_CALL_TRANSACTION;
const TX_ECHO: TransactionCode = FIRST_CALL_TRANSACTION + 1;
const TX_CALLER_CID: TransactionCode = FIRST_CALL_TRANSACTION + 2;
const TX_CALL_BACK: TransactionCode = FIRST_CALL_TRANSACTION + 3;
const TX_CALLED_BACK: TransactionCode = FIRST_CALL_TRANSACTION + 4;
const TX_MAKE: TransactionCode = FIRST_CALL_TRANSACTION + 5;
const TX_LIVE: TransactionCode = FIRST_CALL_TRANSACTION + 6;
const TX_STALL: TransactionCode = FIRST_CALL_TRANSACTION + 7;
const TX_SEQ: TransactionCode = FIRST_CALL_TRANSACTION + 8;
const TX_SEQ_RESULT: TransactionCode = FIRST_CALL_TRANSACTION + 9;
const TX_HANG: TransactionCode = FIRST_CALL_TRANSACTION + 10;
const TX_EXIT: TransactionCode = FIRST_CALL_TRANSACTION + 11;
const TX_KILL_GUEST: TransactionCode = FIRST_CALL_TRANSACTION + 12;
const TX_STOP: TransactionCode = FIRST_CALL_TRANSACTION + 13;

/// How long the frames of `flood` keep the server from reading.
const STALL: Duration = Duration::from_secs(1);
/// `flood`'s frames add up to 6.4 MiB, over the 256 KiB a vsock socket buffers by default.
const FLOOD_FRAMES: u32 = 200;
const FLOOD_FRAME_LEN: usize = 32 * 1024;
/// How long a side waits for what its peer must do by itself, such as boot.
const PEER_DEADLINE: Duration = Duration::from_secs(30);
/// How long a session may take to hear that its peer has gone.
const DEATH_DEADLINE: Duration = Duration::from_secs(5);

const CA: &str = include_str!("tls_fixtures/ca.crt");
const SRV_CRT: &str = include_str!("tls_fixtures/srv.crt");
const SRV_KEY: &str = include_str!("tls_fixtures/srv.key");

fn env_u32(name: &str) -> u32 {
    let v = std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"));
    v.parse().unwrap_or_else(|_| panic!("{name}={v}"))
}
fn peer_cid() -> u32 {
    env_u32("rsbvm_peer_cid")
}
fn port() -> u32 {
    env_u32("rsbvm_port")
}
fn uri(cid: u32) -> Uri {
    Uri::new(Endpoint::vsock(cid, port())).with_wire(WireProfile::Android13Plus(WireVersion::MAX))
}

fn kill_qemu() {
    let file = std::env::var("rsbvm_qemu_pidfile").expect("rsbvm_qemu_pidfile is not set");
    let pid = std::fs::read_to_string(&file).expect("read the pid file");
    let status = std::process::Command::new("kill")
        .args(["-KILL", pid.trim()])
        .status()
        .expect("run kill");
    assert!(status.success(), "kill {pid}: {status}");
}

/// An object whose drop on the server is what its client's release shows up as.
struct Tracked(Arc<AtomicUsize>);
impl Tracked {
    fn new(live: &Arc<AtomicUsize>) -> Self {
        live.fetch_add(1, Ordering::SeqCst);
        Self(live.clone())
    }
}
impl Drop for Tracked {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl Interface for Tracked {}
impl Remotable for Tracked {
    fn descriptor() -> &'static str {
        DESC
    }
    fn on_transact(&self, code: TransactionCode, r: &mut Parcel, reply: &mut Parcel) -> Result<()> {
        match code {
            TX_PING => {
                let s: String = r.read()?;
                reply.write(&Status::from(StatusCode::Ok))?;
                reply.write(&format!("pong:{s}"))
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }
    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}

/// The client's callback object: counts its calls, and its drop shows the server's release.
struct Callback {
    calls: Arc<AtomicUsize>,
    _tracked: Tracked,
}
impl Interface for Callback {}
impl Remotable for Callback {
    fn descriptor() -> &'static str {
        DESC
    }
    fn on_transact(&self, code: TransactionCode, r: &mut Parcel, reply: &mut Parcel) -> Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self._tracked.on_transact(code, r, reply)
    }
    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}

struct Probe {
    stop: Mutex<Option<SyncSender<()>>>,
    live: Arc<AtomicUsize>,
    called_back: Arc<Mutex<Option<std::result::Result<String, StatusCode>>>>,
    next_seq: AtomicU32,
    bad_seq: AtomicU32,
}

impl Probe {
    fn binder(stop: SyncSender<()>) -> SIBinder {
        Interface::as_binder(&Binder::new(Probe {
            stop: Mutex::new(Some(stop)),
            live: Arc::default(),
            called_back: Arc::default(),
            next_seq: AtomicU32::new(0),
            bad_seq: AtomicU32::new(0),
        }))
    }
}

impl Interface for Probe {}
impl Remotable for Probe {
    fn descriptor() -> &'static str {
        DESC
    }
    fn on_transact(&self, code: TransactionCode, r: &mut Parcel, reply: &mut Parcel) -> Result<()> {
        let ok = Status::from(StatusCode::Ok);
        match code {
            TX_PING => {
                let s: String = r.read()?;
                reply.write(&ok)?;
                reply.write(&format!("pong:{s}"))
            }
            TX_ECHO => {
                let bytes: Vec<u8> = r.read()?;
                reply.write(&ok)?;
                reply.write(&bytes)
            }
            TX_CALLER_CID => {
                let cid = match rsbinder::calling_caller() {
                    Some(Caller::Rpc(PeerIdentity::Vsock { cid })) => cid,
                    other => panic!("the caller of a vsock call is {other:?}"),
                };
                reply.write(&ok)?;
                reply.write(&(cid as i32))
            }
            TX_CALL_BACK => {
                let cb: SIBinder = r.read()?;
                let result = self.called_back.clone();
                // A thread of its own: outside every handler, as a callback over `incoming`.
                std::thread::spawn(move || {
                    let twoway = ping(&cb, "outside");
                    let oneway = call(&cb, TX_PING, FLAG_ONEWAY, |d| d.write(&"oneway")).map(drop);
                    *result.lock().unwrap() = Some(oneway.and(twoway));
                });
                reply.write(&ok)
            }
            TX_CALLED_BACK => {
                let got = self.called_back.lock().unwrap().clone();
                reply.write(&ok)?;
                reply.write(&matches!(got.as_ref(), Some(Ok(s)) if s == "pong:outside"))
            }
            TX_MAKE => {
                reply.write(&ok)?;
                reply.write(&Interface::as_binder(&Binder::new(Tracked::new(
                    &self.live,
                ))))
            }
            TX_LIVE => {
                reply.write(&ok)?;
                reply.write(&(self.live.load(Ordering::SeqCst) as i32))
            }
            TX_STALL => {
                std::thread::sleep(STALL);
                Ok(())
            }
            TX_SEQ => {
                let seq: i32 = r.read()?;
                let bytes: Vec<u8> = r.read()?;
                let next = self.next_seq.fetch_add(1, Ordering::SeqCst);
                if seq as u32 != next || bytes.len() != FLOOD_FRAME_LEN {
                    self.bad_seq.fetch_add(1, Ordering::SeqCst);
                }
                Ok(())
            }
            TX_SEQ_RESULT => {
                reply.write(&ok)?;
                reply.write(&(self.next_seq.load(Ordering::SeqCst) as i32))?;
                reply.write(&(self.bad_seq.load(Ordering::SeqCst) as i32))
            }
            TX_HANG => loop {
                std::thread::park();
            },
            TX_EXIT => std::process::exit(42),
            TX_KILL_GUEST => {
                kill_qemu();
                reply.write(&ok)
            }
            TX_STOP => {
                if let Some(stop) = self.stop.lock().unwrap().take() {
                    let _ = stop.send(());
                }
                reply.write(&ok)
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }
    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}

/// A transaction on `binder`; a twoway reply comes back past its status.
fn call(
    binder: &SIBinder,
    code: TransactionCode,
    flags: u32,
    write: impl FnOnce(&mut Parcel) -> Result<()>,
) -> Result<Option<Parcel>> {
    let rp = (**binder)
        .as_any()
        .downcast_ref::<RpcProxy>()
        .expect("RpcProxy");
    let mut d = rp.prepare_request(DESC)?;
    write(&mut d)?;
    let Some(mut r) = rp.submit_transact(code, &d, flags)? else {
        return Ok(None);
    };
    let st: Status = r.read()?;
    if !st.is_ok() {
        return Err(StatusCode::from(st));
    }
    Ok(Some(r))
}
fn twoway(
    binder: &SIBinder,
    code: TransactionCode,
    write: impl FnOnce(&mut Parcel) -> Result<()>,
) -> Result<Parcel> {
    call(binder, code, 0, write)?.ok_or(StatusCode::UnexpectedNull)
}
fn ping(binder: &SIBinder, s: &str) -> std::result::Result<String, StatusCode> {
    twoway(binder, TX_PING, |d| d.write(&s))?.read()
}
fn live_of(probe: &SIBinder) -> i32 {
    twoway(probe, TX_LIVE, |_| Ok(()))
        .expect("TX_LIVE")
        .read()
        .expect("read the count")
}

fn await_true(what: &str, deadline: Duration, mut f: impl FnMut() -> bool) {
    let until = Instant::now() + deadline;
    while !f() {
        assert!(Instant::now() < until, "{what}: not within {deadline:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// `Client::open_with` until the peer has booted far enough to listen.
fn open(incoming: Option<u32>) -> Client {
    let uri = uri(peer_cid());
    let until = Instant::now() + PEER_DEADLINE;
    loop {
        match Client::open_with(&uri, |o, _| o.incoming_connections = incoming) {
            Ok(client) => return client,
            Err(e) if Instant::now() >= until => panic!("open {uri}: {e:?}"),
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}
fn session_of(client: &Client) -> RpcSession {
    client.session().expect("an rpc session").clone()
}

struct Died(SyncSender<()>);
impl DeathRecipient for Died {
    fn binder_died(&self, _who: &WIBinder) {
        let _ = self.0.try_send(());
    }
}

/// Serves `Probe` until its client says `TX_STOP`, then stops within the deadline: a worker that
/// does not end would keep this process, and the VM's exit, from ever coming.
#[test]
#[ignore = "one side of tests/scripts/run_vsock_vm.sh"]
fn serve() {
    let (stop, stopped) = sync_channel(1);
    let guard = rsbinder::serve(uri(VMADDR_CID_ANY))
        .expect("serve vsock://")
        .add("probe", Probe::binder(stop))
        .expect("add")
        .spawn()
        .expect("spawn");
    stopped
        .recv_timeout(Duration::from_secs(120))
        .expect("no TX_STOP from the client");
    drop(guard);
}

/// What only a real peer shows: both cids, frames past the socket buffer, a callback on an
/// incoming connection, and the releases of both directions.
#[test]
#[ignore = "one side of tests/scripts/run_vsock_vm.sh"]
fn call_all() {
    let client = open(Some(1));
    let session = session_of(&client);
    let local = vsock::get_local_cid().expect("get_local_cid");
    match session.peer_identity() {
        PeerIdentity::Vsock { cid } => assert_eq!(cid, peer_cid(), "the peer's cid"),
        other => panic!("expected a vsock peer, got {other}"),
    }
    let probe = client.binder("probe").expect("probe");
    assert_eq!(ping(&probe, "hi").as_deref(), Ok("pong:hi"));

    let caller: i32 = twoway(&probe, TX_CALLER_CID, |_| Ok(()))
        .expect("TX_CALLER_CID")
        .read()
        .expect("read the cid");
    assert_eq!(caller as u32, local, "the cid the server sees for this end");

    for len in [1usize, 4096, 300 * 1024, 1024 * 1024] {
        let bytes: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let back: Vec<u8> = twoway(&probe, TX_ECHO, |d| d.write(&bytes))
            .unwrap_or_else(|e| panic!("echo of {len} bytes: {e:?}"))
            .read()
            .expect("read the echo");
        assert!(back == bytes, "the echo of {len} bytes differs");
    }

    assert!(
        client.caps().contains(rsbinder::TransportCaps::CALLBACKS),
        "an incoming connection grants CALLBACKS"
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let cb_live = Arc::new(AtomicUsize::new(0));
    let cb = Interface::as_binder(&Binder::new(Callback {
        calls: calls.clone(),
        _tracked: Tracked::new(&cb_live),
    }));
    twoway(&probe, TX_CALL_BACK, |d| d.write(&cb)).expect("TX_CALL_BACK");
    await_true("the twoway and the oneway callback", PEER_DEADLINE, || {
        calls.load(Ordering::SeqCst) == 2
    });
    await_true("the server's callback result", PEER_DEADLINE, || {
        twoway(&probe, TX_CALLED_BACK, |_| Ok(()))
            .expect("TX_CALLED_BACK")
            .read::<bool>()
            .expect("read the result")
    });
    drop(cb);
    await_true(
        "the server's release of the callback",
        PEER_DEADLINE,
        || cb_live.load(Ordering::SeqCst) == 0,
    );

    let made: SIBinder = twoway(&probe, TX_MAKE, |_| Ok(()))
        .expect("TX_MAKE")
        .read()
        .expect("read the object");
    assert_eq!(live_of(&probe), 1);
    assert_eq!(ping(&made, "made").as_deref(), Ok("pong:made"));
    drop(made);
    await_true("this end's release of the object", PEER_DEADLINE, || {
        live_of(&probe) == 0
    });

    twoway(&probe, TX_STOP, |_| Ok(())).expect("TX_STOP");
}

/// Oneways past the socket buffer while the server reads nothing, with releases on another thread
/// between them: the credit stops the sender, and every frame arrives whole and in order.
#[test]
#[ignore = "one side of tests/scripts/run_vsock_vm.sh"]
fn flood() {
    let client = open(None);
    let probe = client.binder("probe").expect("probe");
    let objects: Vec<SIBinder> = (0..50)
        .map(|_| {
            twoway(&probe, TX_MAKE, |_| Ok(()))
                .expect("TX_MAKE")
                .read()
                .expect("read the object")
        })
        .collect();
    assert_eq!(live_of(&probe), 50);

    let start = Instant::now();
    call(&probe, TX_STALL, FLAG_ONEWAY, |_| Ok(())).expect("TX_STALL");
    let releases = std::thread::spawn(move || {
        for object in objects {
            drop(object);
            std::thread::sleep(Duration::from_millis(20));
        }
    });
    let payload = vec![0x5au8; FLOOD_FRAME_LEN];
    for seq in 0..FLOOD_FRAMES {
        call(&probe, TX_SEQ, FLAG_ONEWAY, |d| {
            d.write(&(seq as i32))?;
            d.write(&payload)
        })
        .unwrap_or_else(|e| panic!("frame {seq}: {e:?}"));
    }
    let sent_in = start.elapsed();
    releases.join().expect("the release thread");
    assert!(
        sent_in >= STALL / 2,
        "{sent_in:?} to send: the server's stall never stopped the sender"
    );

    let mut r = twoway(&probe, TX_SEQ_RESULT, |_| Ok(())).expect("TX_SEQ_RESULT");
    let next: i32 = r.read().expect("read the count");
    let bad: i32 = r.read().expect("read the bad count");
    assert_eq!((next as u32, bad), (FLOOD_FRAMES, 0), "frames (seen, bad)");
    await_true("the releases of the flood", PEER_DEADLINE, || {
        live_of(&probe) == 0
    });
    twoway(&probe, TX_STOP, |_| Ok(())).expect("TX_STOP");
}

/// The server's process exits in a handler with its machine still up: the reply wait fails, and
/// a linked recipient hears of it on a session nothing reads.
#[test]
#[ignore = "one side of tests/scripts/run_vsock_vm.sh"]
fn peer_exits_during_call() {
    let client = open(None);
    let probe = client.binder("probe").expect("probe");
    let (tx, died) = sync_channel(1);
    let recipient = Arc::new(Died(tx));
    probe
        .link_to_death(Arc::downgrade(&recipient) as _)
        .expect("link_to_death");
    let start = Instant::now();
    let got = twoway(&probe, TX_EXIT, |_| Ok(()));
    println!(
        "reply wait ended in {:?}: {:?}",
        start.elapsed(),
        got.as_ref().map(drop)
    );
    assert_eq!(got.map(drop), Err(StatusCode::DeadObject));
    died.recv_timeout(DEATH_DEADLINE).expect("the recipient");
    assert_eq!(ping(&probe, "after"), Err(StatusCode::DeadObject));
}

/// `f`'s result arrives within the deadline once the VM is killed: a wait the kill does not end
/// fails here rather than hanging the script.
fn kill_and_await<T: Send + 'static>(what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = sync_channel(1);
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    std::thread::sleep(Duration::from_millis(300));
    let start = Instant::now();
    kill_qemu();
    let got = rx
        .recv_timeout(DEATH_DEADLINE)
        .unwrap_or_else(|_| panic!("{what}: not within {DEATH_DEADLINE:?} of the kill"));
    println!("{what}: {:?} after the kill", start.elapsed());
    got
}

#[test]
#[ignore = "the host's side of tests/scripts/run_vsock_vm.sh"]
fn guest_killed_during_call() {
    let client = open(None);
    let probe = client.binder("probe").expect("probe");
    assert_eq!(ping(&probe, "before").as_deref(), Ok("pong:before"));
    let (tx, died) = sync_channel(1);
    let recipient = Arc::new(Died(tx));
    probe
        .link_to_death(Arc::downgrade(&recipient) as _)
        .expect("link_to_death");
    let hung = probe.clone();
    let got = kill_and_await("the reply wait", move || {
        twoway(&hung, TX_HANG, |_| Ok(())).map(drop)
    });
    assert_eq!(got, Err(StatusCode::DeadObject));
    died.recv_timeout(DEATH_DEADLINE).expect("the recipient");
    assert_eq!(ping(&probe, "after"), Err(StatusCode::DeadObject));
}

/// A session nothing reads: only the peer watch can hear of this.
#[test]
#[ignore = "the host's side of tests/scripts/run_vsock_vm.sh"]
fn guest_killed_idle_session() {
    let client = open(None);
    let probe = client.binder("probe").expect("probe");
    assert_eq!(ping(&probe, "before").as_deref(), Ok("pong:before"));
    let (tx, died) = sync_channel(1);
    let recipient = Arc::new(Died(tx));
    probe
        .link_to_death(Arc::downgrade(&recipient) as _)
        .expect("link_to_death");
    kill_and_await("the idle session's recipient", move || {
        died.recv_timeout(DEATH_DEADLINE).expect("the recipient")
    });
    assert_eq!(ping(&probe, "after"), Err(StatusCode::DeadObject));
}

/// A killed VM reads as its close, not as an error of the socket: between frames the stream stays
/// `InSync`, so a caller's `into_result` cannot tell this from a peer that ended on purpose.
#[test]
#[ignore = "the host's side of tests/scripts/run_vsock_vm.sh"]
fn guest_killed_serve_loop() {
    let client = open(None);
    let session = session_of(&client);
    let probe = client.binder("probe").expect("probe");
    assert_eq!(ping(&probe, "before").as_deref(), Ok("pong:before"));
    let serve = session.spawn_serve().expect("spawn_serve");
    let end = kill_and_await("the serve loop", move || {
        serve.join().expect("the serve thread")
    });
    println!("serve loop ended with {end:?}");
    assert_eq!(end.reason, EndReason::EndOfStream);
    assert!(end.is_clean(), "{end:?}");
    assert_eq!(ping(&probe, "after"), Err(StatusCode::DeadObject));
}

#[test]
#[ignore = "the host's side of tests/scripts/run_vsock_vm.sh"]
fn guest_killed_incoming_connections() {
    let client = open(Some(1));
    let session = session_of(&client);
    let probe = client.binder("probe").expect("probe");
    assert_eq!(ping(&probe, "before").as_deref(), Ok("pong:before"));
    let (tx, died) = sync_channel(1);
    let recipient = Arc::new(Died(tx));
    probe
        .link_to_death(Arc::downgrade(&recipient) as _)
        .expect("link_to_death");
    kill_and_await("the recipient", move || {
        died.recv_timeout(DEATH_DEADLINE).expect("the recipient")
    });
    await_true("the incoming threads' end", DEATH_DEADLINE, || {
        session.__incoming_thread_live_count() == 0
    });
    assert_eq!(ping(&probe, "after"), Err(StatusCode::DeadObject));
}

/// A send blocked on the peer's credit, with no reader to fail first: the kill must reach it.
#[test]
#[ignore = "the host's side of tests/scripts/run_vsock_vm.sh"]
fn guest_killed_while_send_blocks() {
    let client = open(None);
    let probe = client.binder("probe").expect("probe");
    call(&probe, TX_HANG, FLAG_ONEWAY, |_| Ok(())).expect("TX_HANG");
    let sender = probe.clone();
    let sent = Arc::new(AtomicUsize::new(0));
    let count = sent.clone();
    let got = kill_and_await("the blocked send", move || {
        let payload = vec![0x5au8; FLOOD_FRAME_LEN];
        for seq in 0..1024 * 1024_i32 {
            call(&sender, TX_SEQ, FLAG_ONEWAY, |d| {
                d.write(&seq)?;
                d.write(&payload)
            })?;
            count.fetch_add(1, Ordering::SeqCst);
        }
        Ok::<(), StatusCode>(())
    });
    println!("{} frames went out", sent.load(Ordering::SeqCst));
    assert_eq!(got, Err(StatusCode::DeadObject));
}

/// The guest's side of the host server that outlives a VM: never returns, the handler kills it.
#[test]
#[ignore = "one side of tests/scripts/run_vsock_vm.sh"]
fn request_kill() {
    let client = open(None);
    let probe = client.binder("probe").expect("probe");
    let got = twoway(&probe, TX_KILL_GUEST, |_| Ok(())).map(drop);
    panic!("still running after TX_KILL_GUEST: {got:?}");
}

fn certs(pem: &str) -> Vec<CertificateDer<'static>> {
    CertificateDer::pem_slice_iter(pem.as_bytes())
        .collect::<std::result::Result<_, _>>()
        .expect("parse certs")
}

#[test]
#[ignore = "one side of tests/scripts/run_vsock_vm.sh"]
fn tls_serve() {
    let cfg = Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                certs(SRV_CRT),
                PrivateKeyDer::from_pem_slice(SRV_KEY.as_bytes()).expect("parse key"),
            )
            .expect("server config"),
    );
    let server = RpcServer::setup_vsock_server_tls(VMADDR_CID_ANY, port(), cfg)
        .expect("setup_vsock_server_tls");
    let (stop, stopped) = sync_channel(1);
    server.set_root(Probe::binder(stop)).expect("set_root");
    let bg = server.run_background().expect("run_background");
    stopped
        .recv_timeout(Duration::from_secs(120))
        .expect("no TX_STOP from the client");
    server.stop_accepting();
    server.join_workers();
    bg.stop_and_join().expect("accept loop");
}

#[test]
#[ignore = "one side of tests/scripts/run_vsock_vm.sh"]
fn tls_call() {
    let mut roots = RootCertStore::empty();
    for c in certs(CA) {
        roots.add(c).expect("add ca");
    }
    let cfg = Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let until = Instant::now() + PEER_DEADLINE;
    let addr = vsock::VsockAddr::new(peer_cid(), port());
    let stream = loop {
        match vsock::VsockStream::connect(&addr) {
            Ok(stream) => break stream,
            Err(e) if Instant::now() >= until => panic!("connect {addr:?}: {e}"),
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    let t = TlsTransport::connect_stream(Box::new(stream), "localhost", cfg).expect("handshake");
    match t.peer_identity() {
        PeerIdentity::Certificate(c) => assert_eq!(c.fingerprint().len(), 32),
        other => panic!("expected a certificate peer over vsock+TLS, got {other}"),
    }
    let session = RpcSession::new(Box::new(t), AddressSpace::Initiator).expect("RpcSession::new");
    let root = session.get_root().expect("get_root");
    assert_eq!(ping(&root, "tls").as_deref(), Ok("pong:tls"));
    let bytes: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
    let back: Vec<u8> = twoway(&root, TX_ECHO, |d| d.write(&bytes))
        .expect("echo")
        .read()
        .expect("read the echo");
    assert!(back == bytes, "the echo over TLS differs");
    twoway(&root, TX_STOP, |_| Ok(())).expect("TX_STOP");
}
