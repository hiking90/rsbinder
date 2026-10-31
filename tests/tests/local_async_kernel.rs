// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 17 K-4 / K-20: a service published sync-only (`BnLocalSvc::new_binder`) and called
//! through its async view, which drives the local binder through the in-process transport.
//!
//! - Off any transaction the call runs on a pool thread and the handler sees
//!   `Caller::InProcess` (this process); `is_handling_transaction()` stays `false`.
//! - Inside a kernel handler the call runs inline on the handler's thread and the handler sees
//!   the outer kernel caller, also through a pool that always hands off.
//! - Binder and fd arguments, a oneway method and `@EnforcePermission` go through the same path.
//! - The strict-mode policy and work source the inner dispatch installs are restored on the
//!   calling thread.
//! - A local binder of another `Remotable` with the same descriptor is proxied in-process
//!   (plan 17 D-13).
//!
//! The inline case needs a kernel transaction, so the caller is this test binary re-executed as
//! `child` with [`ROLE`] set; it looks the outer service up through the service manager.
//!
//! Requires a binder device and a service manager that accepts `addService` for any name
//! (`rsb_hub --config tests/policy/permissive.toml`):
//!
//! ```text
//! cargo test -p tests --test local_async_kernel -- --ignored --test-threads=1
//! ```

#![cfg(any(target_os = "linux", target_os = "android"))]

use std::io::{BufRead, BufReader, Write as _};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use rsbinder::permission_controller::{set_permission_authority, PermissionAuthority};
use rsbinder::{
    hub, BinderAsyncPool, BinderResult, BoxFuture, Caller, Interface, ParcelFileDescriptor,
    ProcessState, SIBinder, Status, StatusCode, Strong, Tokio,
};

include!(concat!(env!("OUT_DIR"), "/local_async.rs"));

use localasync::ILocalOuter::{BnLocalOuter, ILocalOuter};
use localasync::ILocalSvc::{BnLocalSvc, ILocalSvc, ILocalSvcAsync};

const ROLE: &str = "RSB_LOCAL_ASYNC_ROLE";
const NAME: &str = "RSB_LOCAL_ASYNC_NAME";
/// The prefix of the child's report line.
const REPORT: &str = "LOCAL_ASYNC_REPORT ";

/// The pid of the child whose kernel calls the permission policy grants.
static CHILD_PID: AtomicI32 = AtomicI32::new(-1);

fn own_pid() -> i32 {
    std::process::id() as i32
}

fn own_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// Grants an in-process call of this process and a kernel call of [`CHILD_PID`], nothing else.
struct Policy;

impl PermissionAuthority for Policy {
    fn check(&self, _permission: &str, caller: &Caller) -> bool {
        match caller {
            Caller::InProcess { uid, pid } => *uid == own_uid() && *pid == own_pid(),
            Caller::Kernel { pid, .. } => *pid == CHILD_PID.load(Ordering::SeqCst),
            _ => false,
        }
    }
}

fn init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        ProcessState::init_default().expect("a binder device");
        ProcessState::start_thread_pool();
        set_permission_authority(Arc::new(Policy));
    });
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .build()
        .expect("a Tokio runtime")
}

fn thread_id() -> String {
    format!("{:?}", std::thread::current().id())
}

fn thread_state() -> String {
    format!(
        "{:#x}:{}",
        rsbinder::get_strict_mode_policy(),
        rsbinder::get_calling_work_source_uid()
    )
}

#[derive(Default)]
struct Svc {
    bumps: AtomicI32,
}

impl Interface for Svc {}

impl ILocalSvc for Svc {
    fn r#callerKind(&self) -> BinderResult<String> {
        Ok(match rsbinder::calling_caller() {
            Some(Caller::Kernel { pid, .. }) => format!("kernel:{pid}"),
            Some(Caller::InProcess { uid, pid }) => format!("inprocess:{uid}:{pid}"),
            Some(_) => "rpc".to_string(),
            None => "none".to_string(),
        })
    }
    fn r#handlingTransaction(&self) -> BinderResult<bool> {
        Ok(rsbinder::is_handling_transaction())
    }
    fn r#threadId(&self) -> BinderResult<String> {
        Ok(thread_id())
    }
    fn r#threadState(&self) -> BinderResult<String> {
        Ok(thread_state())
    }
    fn r#echoBinder(&self, b: &SIBinder) -> BinderResult<SIBinder> {
        Ok(b.clone())
    }
    fn r#readFd(&self, fd: &ParcelFileDescriptor) -> BinderResult<String> {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut &*fd, &mut text)
            .map_err(|_| Status::from(StatusCode::BadValue))?;
        Ok(text)
    }
    fn r#bump(&self) -> BinderResult<()> {
        self.bumps.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn r#bumps(&self) -> BinderResult<i32> {
        Ok(self.bumps.load(Ordering::SeqCst))
    }
    fn r#guarded(&self) -> BinderResult<bool> {
        Ok(true)
    }
}

/// Runs `spawn_me` on the calling thread, so the in-process transact runs there too.
enum InlinePool {}

impl BinderAsyncPool for InlinePool {
    fn spawn<'a, F1, F2, Fut, A, B, E>(spawn_me: F1, after_spawn: F2) -> BoxFuture<'a, Result<B, E>>
    where
        F1: FnOnce() -> A,
        F2: FnOnce(A) -> Fut,
        Fut: std::future::Future<Output = Result<B, E>>,
        F1: Send + 'static,
        F2: Send + 'a,
        Fut: Send + 'a,
        A: Send + 'static,
        B: Send + 'a,
        E: From<StatusCode>,
    {
        let result = spawn_me();
        Box::pin(after_spawn(result))
    }
}

static HAND_OFFS: AtomicUsize = AtomicUsize::new(0);

/// Always hands `spawn_me` to a new thread, as the `BinderAsyncPool` doc tells a pool to.
enum HandOffPool {}

impl BinderAsyncPool for HandOffPool {
    fn spawn<'a, F1, F2, Fut, A, B, E>(spawn_me: F1, after_spawn: F2) -> BoxFuture<'a, Result<B, E>>
    where
        F1: FnOnce() -> A,
        F2: FnOnce(A) -> Fut,
        Fut: std::future::Future<Output = Result<B, E>>,
        F1: Send + 'static,
        F2: Send + 'a,
        Fut: Send + 'a,
        A: Send + 'static,
        B: Send + 'a,
        E: From<StatusCode>,
    {
        HAND_OFFS.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let _ = tx.send(spawn_me());
        });
        Box::pin(async move {
            match rx.await {
                Ok(result) => after_spawn(result).await,
                Err(_) => Err(StatusCode::DeadObject.into()),
            }
        })
    }
}

/// Off any transaction the async view runs the service on a pool thread as this process.
#[test]
#[ignore = "needs a binder device; see the module doc"]
fn async_view_off_transaction_runs_on_the_pool_as_this_process() {
    init();
    let sync = BnLocalSvc::new_binder(Svc::default());
    let svc: Strong<dyn ILocalSvcAsync<Tokio>> = sync.clone().into_async::<Tokio>();
    let rt = runtime();
    let caller_thread = thread_id();
    rt.block_on(async {
        assert_eq!(
            svc.callerKind().await,
            Ok(format!("inprocess:{}:{}", own_uid(), own_pid()))
        );
        assert_eq!(svc.handlingTransaction().await, Ok(false));
        assert_ne!(svc.threadId().await.expect("threadId"), caller_thread);
        // `Policy` grants `Caller::InProcess` of this process.
        assert_eq!(svc.guarded().await, Ok(true));
    });
    // The frame is gone once the call returned.
    assert!(rsbinder::calling_caller().is_none());
    // The sync handle is a direct call: no dispatch, no caller.
    assert_eq!(sync.callerKind(), Ok("none".to_string()));
}

/// Binder and fd arguments and a oneway method go through the in-process transport.
#[test]
#[ignore = "needs a binder device; see the module doc"]
fn async_view_carries_binder_and_fd_arguments_and_oneway() {
    init();
    let svc = BnLocalSvc::new_binder(Svc::default()).into_async::<Tokio>();
    let rt = runtime();
    rt.block_on(async {
        let other: SIBinder = BnLocalSvc::new_binder(Svc::default()).as_binder();
        let echoed = svc.echoBinder(&other).await.expect("echoBinder");
        assert_eq!(echoed, other);
        assert!(other.was_parceled(), "a binder argument is marked parceled");

        let (reader, writer) = rustix::pipe::pipe().expect("pipe");
        let mut writer = std::fs::File::from(writer);
        writer.write_all(b"through the parcel").expect("write");
        drop(writer);
        let fd = ParcelFileDescriptor::new(reader);
        assert_eq!(svc.readFd(&fd).await, Ok("through the parcel".to_string()));

        svc.bump().await.expect("oneway bump");
        svc.bump().await.expect("oneway bump");
        // A local oneway returns once the method has run.
        assert_eq!(svc.bumps().await, Ok(2));
    });
}

/// The calling thread's strict-mode policy and work source come back after the dispatch.
#[test]
#[ignore = "needs a binder device; see the module doc"]
fn in_process_dispatch_restores_strict_mode_and_work_source() {
    init();
    let svc = BnLocalSvc::new_binder(Svc::default()).into_async::<InlinePool>();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a Tokio runtime");

    rsbinder::set_strict_mode_policy(0x10);
    let token = rsbinder::set_calling_work_source_uid(4321);
    rsbinder::clear_propagate_work_source();

    let (inside, inside_thread) = rt.block_on(async {
        (
            svc.threadState().await.expect("threadState"),
            svc.threadId().await.expect("threadId"),
        )
    });
    assert_eq!(
        inside_thread,
        thread_id(),
        "InlinePool runs the dispatch here"
    );
    // The header carried the policy with the gather bit and no work source.
    assert_eq!(inside, format!("{:#x}:{}", 0x10 | (1u32 << 31), u32::MAX));
    assert_eq!(rsbinder::get_strict_mode_policy(), 0x10);
    assert_eq!(rsbinder::get_calling_work_source_uid(), 4321);
    assert!(!rsbinder::should_propagate_work_source());

    rsbinder::restore_calling_work_source(token);
    rsbinder::set_strict_mode_policy(0);
}

/// A local binder of another `Remotable` with the descriptor is proxied in-process (D-13).
#[test]
#[ignore = "needs a binder device; see the module doc"]
fn local_binder_of_another_remotable_is_proxied_in_process() {
    struct Alias;
    impl rsbinder::Remotable for Alias {
        fn descriptor() -> &'static str {
            "localasync.ILocalSvc"
        }
        fn on_transact(
            &self,
            code: rsbinder::TransactionCode,
            _reader: &mut rsbinder::Parcel,
            reply: &mut rsbinder::Parcel,
        ) -> rsbinder::Result<()> {
            // `callerKind` is the first method.
            if code != rsbinder::FIRST_CALL_TRANSACTION {
                return Err(StatusCode::UnknownTransaction);
            }
            reply.write(&Status::from(StatusCode::Ok))?;
            reply.write(&"alias".to_string())
        }
        fn on_dump(&self, _: &mut dyn std::io::Write, _: &[String]) -> rsbinder::Result<()> {
            Ok(())
        }
    }

    init();
    let binder = rsbinder::Binder::new(Alias).as_binder();
    let sync: Strong<dyn ILocalSvc> = binder.clone().into_interface().expect("sync cast");
    assert_eq!(sync.callerKind(), Ok("alias".to_string()));
    let svc: Strong<dyn ILocalSvcAsync<Tokio>> = binder.into_interface().expect("async cast");
    // Called inside the runtime: `Tokio::spawn` hands off when the method is called.
    let kind = runtime().block_on(async { svc.callerKind().await });
    assert_eq!(kind, Ok("alias".to_string()));
}

struct Outer {
    tokio: Strong<dyn ILocalSvcAsync<Tokio>>,
    hand_off: Strong<dyn ILocalSvcAsync<HandOffPool>>,
}

impl Interface for Outer {}

impl ILocalOuter for Outer {
    fn r#probeInner(&self) -> BinderResult<String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .map_err(|_| Status::from(StatusCode::NoMemory))?;
        let here = thread_id();
        let policy = rsbinder::get_strict_mode_policy();
        rsbinder::set_strict_mode_policy(0x10);
        let token = rsbinder::set_calling_work_source_uid(4321);
        let hand_offs = HAND_OFFS.load(Ordering::SeqCst);
        let report = rt.block_on(async {
            let kind = self.tokio.callerKind().await?;
            let handling = self.tokio.handlingTransaction().await?;
            let inline = self.tokio.threadId().await? == here;
            let guarded = self.tokio.guarded().await?;
            let state = self.tokio.threadState().await?;
            let hand_off_inline = self.hand_off.threadId().await? == here;
            Ok::<_, Status>(format!(
                "kind={kind} handling={handling} inline={inline} guarded={guarded} \
                 state={state} hand_off_inline={hand_off_inline}"
            ))
        })?;
        let report = format!(
            "{report} hand_offs={} after={}",
            HAND_OFFS.load(Ordering::SeqCst) - hand_offs,
            thread_state()
        );
        rsbinder::restore_calling_work_source(token);
        rsbinder::set_strict_mode_policy(policy);
        Ok(report)
    }
}

/// The caller role: waits for `go` on stdin, calls `probeInner`, prints [`REPORT`] and the text.
#[test]
#[ignore = "a child role of the test below"]
fn child() {
    let Ok(name) = std::env::var(NAME) else {
        return;
    };
    ProcessState::init_default().expect("a binder device");
    let outer: Strong<dyn ILocalOuter> = hub::try_get_interface(&name)
        .expect("the service manager")
        .expect("the outer service");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).expect("stdin");
    let report = outer.probeInner().expect("probeInner");
    // On its own line: libtest has already printed `test child ... ` without a newline.
    println!("\n{REPORT}{report}");
}

/// Inside a kernel handler the async view runs inline, with the outer caller, whatever the pool.
#[test]
#[ignore = "needs a binder device and a permissive service manager; see the module doc"]
fn async_view_inside_a_kernel_handler_runs_inline_with_the_outer_caller() {
    init();
    let inner = BnLocalSvc::new_binder(Svc::default());
    let outer = BnLocalOuter::new_binder(Outer {
        tokio: inner.clone().into_async::<Tokio>(),
        hand_off: inner.into_async::<HandOffPool>(),
    });
    let name = format!("rsb.test.localasync.{}", std::process::id());
    hub::add_service(&name, outer.as_binder()).expect("addService");

    let exe = std::env::current_exe().expect("current_exe");
    let mut child = Command::new(exe)
        .args([
            "--ignored",
            "--exact",
            "child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ROLE, "child")
        .env(NAME, &name)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the child");
    let child_pid = child.id() as i32;
    CHILD_PID.store(child_pid, Ordering::SeqCst);
    let mut stdin = child.stdin.take().expect("child stdin");
    writeln!(stdin, "go").expect("tell the child to go");

    let stdout = child.stdout.take().expect("child stdout");
    let report: Arc<Mutex<Option<String>>> = Arc::default();
    let reader = {
        let report = report.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(text) = line.strip_prefix(REPORT) {
                    *report.lock().unwrap() = Some(text.to_string());
                }
            }
        })
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the child did not finish");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    reader.join().expect("the stdout reader");
    assert!(status.success(), "the child failed: {status}");

    let report = report
        .lock()
        .unwrap()
        .clone()
        .expect("a report from the child");
    let expected = format!(
        "kind=kernel:{child_pid} handling=true inline=true guarded=true state={:#x}:4321 \
         hand_off_inline=true hand_offs=0 after=0x10:4321",
        0x10 | (1u32 << 31)
    );
    assert_eq!(report, expected);
}
