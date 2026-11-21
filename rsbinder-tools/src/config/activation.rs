// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Starting a declared service on demand.
//!
//! AOSP's `tryStartService` sets the `ctl.interface_start` property from a
//! detached thread and lets init do the work. Two properties of that are
//! worth copying exactly: it never blocks the transaction, and it never
//! waits for the service to appear — the client is already waiting on a
//! registration notification, which is what actually tells it the service
//! is up.

use std::os::fd::AsFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::declaration::Activation;
use super::trust::{check_program, open_root, At};

/// The name of the thread each start runs on. It makes no binder call, and a
/// runner that panics there frees its slot and ends only that start.
pub const START_THREAD_NAME: &str = "rsb_hub:start";

/// Runs one activation to completion. Injectable so the debounce can be
/// tested without spawning processes.
pub trait ActivationRunner: Send + Sync {
    /// Bring `name` up. Called on a dedicated thread, so blocking here is
    /// expected — for [`Activation::Exec`] it blocks for the service's
    /// whole lifetime.
    fn run(&self, name: &str, activation: &Activation);
}

/// By failures in a row: 1 s is `wait_for_service`'s poll, 30 s bounds a repaired program's wait.
const RETRY_AFTER_SECS: [u64; 6] = [1, 2, 4, 8, 16, 30];

/// The hold-off after `failures` ends in a row; `failures` is at least 1.
fn retry_after(failures: u32) -> Duration {
    let index = usize::try_from(failures.saturating_sub(1)).unwrap_or(usize::MAX);
    let secs = RETRY_AFTER_SECS.get(index).copied().unwrap_or(30);
    Duration::from_secs(secs)
}

type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// A start that is outstanding.
struct Start {
    name: String,
    activation: Activation,
    /// A name of the activation was registered since the start began.
    registered: bool,
}

/// An `exec` activation whose last start ended with nothing registered.
struct Failing {
    activation: Activation,
    failures: u32,
    /// No start of the activation before this.
    retry_at: Instant,
}

#[derive(Default)]
struct State {
    /// A match on either half blocks a new start (see [`Activator::try_start`]).
    in_flight: Vec<Start>,
    /// Only activations held off: a registration removes the entry.
    failing: Vec<Failing>,
}

/// What [`Activator::status`] reports of one activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct StartStatus {
    /// A start of it has not ended: for `exec`, the program is running.
    pub outstanding: bool,
    /// Starts in a row that ended with nothing registered; 0 once a name of
    /// it is registered.
    pub failures: u32,
    /// How long the next start is still held off, if it is.
    pub retry_in: Option<Duration>,
}

/// Starts declared services, at most one attempt at a time per name and per activation.
pub struct Activator {
    state: Arc<Mutex<State>>,
    runner: Arc<dyn ActivationRunner>,
    clock: Clock,
}

impl Activator {
    /// An activator driving `runner`.
    pub fn new(runner: Arc<dyn ActivationRunner>) -> Self {
        Self::with_clock(runner, Arc::new(Instant::now))
    }

    fn with_clock(runner: Arc<dyn ActivationRunner>, clock: Clock) -> Self {
        Activator {
            state: Arc::new(Mutex::new(State::default())),
            runner,
            clock,
        }
    }

    /// Ask for `name` to be started, unless a start of `name` or of the
    /// same `activation` is already outstanding. The activation half covers
    /// another declaration sharing it, as init runs one service for several
    /// interfaces. The name half covers a reload that changed `name`'s
    /// `start` while the old one is still running: the `Activator` outlives
    /// the reload, and a second process would overwrite the first's
    /// registration. Returns immediately; the work happens on its own
    /// thread.
    ///
    /// An [`Activation::Exec`] start that ends with none of its names
    /// registered since it began ([`Self::registered`]) holds the next start
    /// of that activation off: 1, 2, 4, 8, 16, then 30 seconds, by the count
    /// of such ends in a row. The exit code is not what is judged, so a
    /// program that exits 0 at once is held off too, and one that runs only
    /// while it has clients is not. A program that could not be spawned
    /// counts the same. A registration clears the count, so a program that
    /// registers and then exits is started again by the next lookup. An
    /// [`Activation::Systemd`] start is not held off: `systemctl --no-block`
    /// always ends at once, and the unit has `StartLimit*=`.
    ///
    /// Nothing here reports success. A start that fails is logged and
    /// forgotten: the caller is a `getService` that has already answered
    /// "not registered", and the client's own wait is what will notice the
    /// service appearing.
    pub fn try_start(&self, name: &str, activation: &Activation) {
        {
            let mut state = self.state.lock().expect("activator lock poisoned");
            if state
                .in_flight
                .iter()
                .any(|s| s.name == name || &s.activation == activation)
            {
                log::debug!(
                    "not starting {name}: a start of it or of its activation is outstanding"
                );
                return;
            }
            let now = (self.clock)();
            if let Some(failing) = state.failing.iter().find(|f| &f.activation == activation) {
                if now < failing.retry_at {
                    log::debug!(
                        "not starting {name}: its last {} start(s) registered nothing, the next \
                         is held off for {:?}",
                        failing.failures,
                        failing.retry_at - now
                    );
                    return;
                }
            }
            state.in_flight.push(Start {
                name: name.to_owned(),
                activation: activation.clone(),
                registered: false,
            });
        }

        let runner = Arc::clone(&self.runner);
        let slot = InFlightSlot {
            state: Arc::clone(&self.state),
            clock: Arc::clone(&self.clock),
            name: name.to_owned(),
            activation: activation.clone(),
        };
        let spawned = std::thread::Builder::new()
            .name(START_THREAD_NAME.to_owned())
            .spawn(move || {
                // Moved in whole: the slot is freed, and the start judged, on a panic too.
                let slot = slot;
                log::info!("{} is not registered; starting it on demand", slot.name);
                runner.run(&slot.name, &slot.activation);
            });
        if let Err(e) = spawned {
            // The closure, and the slot in it, dropped with the error: the slot is free.
            log::error!("failed to spawn the start thread for {name}: {e}");
        }
    }

    /// Where `activation` stands, for a dump: whether a start of it is
    /// outstanding, and the hold-off its last ends left.
    pub fn status(&self, activation: &Activation) -> StartStatus {
        let state = self.state.lock().expect("activator lock poisoned");
        let now = (self.clock)();
        let failing = state.failing.iter().find(|f| &f.activation == activation);
        StartStatus {
            outstanding: state.in_flight.iter().any(|s| &s.activation == activation),
            failures: failing.map_or(0, |f| f.failures),
            retry_in: failing
                .map(|f| f.retry_at.saturating_duration_since(now))
                .filter(|left| !left.is_zero()),
        }
    }

    /// A name `activation` starts was registered: an outstanding start of it
    /// is not one to hold the next off for, and a hold-off in force ends.
    /// `rsb_hub` calls this from `addService`, for the instance's own name
    /// and for its declared accessor's.
    pub fn registered(&self, activation: &Activation) {
        let mut state = self.state.lock().expect("activator lock poisoned");
        for start in &mut state.in_flight {
            if &start.activation == activation {
                start.registered = true;
            }
        }
        state.failing.retain(|f| &f.activation != activation);
    }
}

/// Frees a start's in-flight slot on drop, so a panicking runner does not wedge it, and judges
/// the start there: an `exec` that ended with nothing registered holds the next one off.
struct InFlightSlot {
    state: Arc<Mutex<State>>,
    clock: Clock,
    name: String,
    activation: Activation,
}

impl Drop for InFlightSlot {
    fn drop(&mut self) {
        // Poison-tolerant: a panic here during unwind would abort the whole hub.
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(at) = state
            .in_flight
            .iter()
            .position(|s| s.name == self.name && s.activation == self.activation)
        else {
            return;
        };
        let start = state.in_flight.swap_remove(at);
        if start.registered || !matches!(start.activation, Activation::Exec(_)) {
            return;
        }
        let now = (self.clock)();
        let known = state
            .failing
            .iter()
            .position(|f| f.activation == start.activation);
        let failures = known
            .map_or(0, |at| state.failing.swap_remove(at).failures)
            .saturating_add(1);
        let hold = retry_after(failures);
        state.failing.push(Failing {
            activation: start.activation,
            failures,
            retry_at: now + hold,
        });
        log::warn!(
            "the start of {} ended with nothing registered ({failures} in a row); the next \
             start of it is held off for {hold:?}",
            self.name
        );
    }
}

/// Runs activations for real.
///
/// An [`Activation::Exec`] program is walked from `/` again right before it
/// is spawned, with the check [`load`](super::load) applies to it
/// ([`ConfigError::UntrustedExec`](super::ConfigError::UntrustedExec)); a
/// program that now fails it is not run, and the start is logged as refused.
/// A failed SIGHUP reload leaves the previous declarations in force, so this
/// is what stops a program that reload judged untrusted from running.
pub struct SystemRunner;

impl SystemRunner {
    /// Absolute paths only: a `PATH` lookup would let an earlier entry run as root.
    fn systemctl() -> Option<&'static str> {
        ["/usr/bin/systemctl", "/bin/systemctl"]
            .into_iter()
            .find(|p| Path::new(p).exists())
    }
}

impl ActivationRunner for SystemRunner {
    fn run(&self, name: &str, activation: &Activation) {
        let mut command = match activation {
            Activation::Systemd(unit) => {
                let Some(systemctl) = Self::systemctl() else {
                    log::error!("cannot start {name}: no systemctl on this host");
                    return;
                };
                // `--no-block`: the client waits for a registration notification, not us.
                let mut c = child_command(systemctl);
                c.arg("--no-block").arg("start").arg("--").arg(unit);
                c
            }
            Activation::Exec(argv) => {
                let root = match open_root(Path::new("/")) {
                    Ok(root) => root,
                    Err(e) => {
                        log::error!("cannot start {name}: cannot open `/`: {e}");
                        return;
                    }
                };
                let root = At {
                    fd: root.as_fd(),
                    shown: Path::new("/"),
                };
                let our_uid = rustix::process::getuid().as_raw();
                match exec_command(root, argv, our_uid) {
                    Ok(c) => c,
                    Err(why) => {
                        log::error!("cannot start {name}: {why}");
                        return;
                    }
                }
            }
        };

        // Wait, not detach: reaps the child, and for `Exec` holds the in-flight slot while it runs.
        match command.spawn() {
            Ok(mut child) => match child.wait() {
                Ok(status) if status.success() => {
                    log::info!("start of {name} finished: {status}")
                }
                Ok(status) => log::warn!("start of {name} exited with {status}"),
                Err(e) => log::error!("waiting on the start of {name} failed: {e}"),
            },
            Err(e) => log::error!("could not start {name}: {e}"),
        }
    }
}

/// The command for an `exec` start, built only once the program passes the load-time check again.
fn exec_command(root: At<'_>, argv: &[String], our_uid: u32) -> Result<Command, String> {
    // Parse checks this too; a hand-built `Activation` must not reach a `PATH` search.
    let Some(program) = argv.first().filter(|p| Path::new(p).is_absolute()) else {
        return Err("`exec` program must be an absolute path".to_owned());
    };
    match check_program(root, Path::new(program), our_uid) {
        Ok(Ok(())) => {}
        Ok(Err(bad)) => {
            return Err(format!(
                "`exec` program {program:?} is not trusted: {} is {}",
                bad.path.display(),
                bad.problem
            ))
        }
        Err(e) => return Err(format!("cannot check `exec` program {program:?}: {e}")),
    }
    let mut c = child_command(program);
    c.args(&argv[1..]);
    Ok(c)
}

/// `program` without the hub's `$NOTIFY_SOCKET`, which `Notifier::from_environment` leaves set,
/// and without its signal mask: `Command` keeps a mask, and `rsb_hub` blocks what `sigwait` takes.
fn child_command(program: &str) -> Command {
    let mut c = Command::new(program);
    c.env_remove(crate::notify::NOTIFY_SOCKET);
    // SAFETY: `unblock_all_signals` allocates nothing and calls only async-signal-safe functions.
    unsafe { c.pre_exec(unblock_all_signals) };
    c
}

/// Between `fork` and `exec`, so async-signal-safe only.
fn unblock_all_signals() -> std::io::Result<()> {
    // SAFETY: `set` is local and initialized by `sigemptyset`; a null `oldset` is allowed.
    let rc = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::pthread_sigmask(libc::SIG_SETMASK, &set, std::ptr::null_mut())
    };
    match rc {
        0 => Ok(()),
        rc => Err(std::io::Error::from_raw_os_error(rc)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// The hub blocks what `sigwait` takes; a child inheriting that would ignore `SIGTERM`.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_child_starts_with_no_signal_blocked() {
        // A thread of its own: the mask is per thread, and this one is never unblocked.
        let out = std::thread::spawn(|| {
            // SAFETY: `set` is local and initialized by `sigemptyset`; a null `oldset` is allowed.
            let blocked = unsafe {
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::sigaddset(&mut set, libc::SIGTERM);
                libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut())
            };
            assert_eq!(blocked, 0);
            child_command("/bin/sh")
                .args(["-c", "grep SigBlk /proc/self/status"])
                .output()
                .expect("spawn")
        })
        .join()
        .expect("thread");
        let line = String::from_utf8(out.stdout).expect("utf-8");
        let mask = line.split_whitespace().nth(1).expect("a SigBlk line");
        assert_eq!(u64::from_str_radix(mask, 16), Ok(0), "{line}");
    }

    struct Recorder {
        started: mpsc::Sender<String>,
        release: Arc<Mutex<bool>>,
    }

    impl ActivationRunner for Recorder {
        fn run(&self, name: &str, _activation: &Activation) {
            self.started.send(name.to_owned()).unwrap();
            // Hold the slot until the test says otherwise.
            while !*self.release.lock().unwrap() {
                std::thread::yield_now();
            }
        }
    }

    #[test]
    fn a_second_request_while_one_is_in_flight_is_dropped() {
        let (tx, rx) = mpsc::channel();
        let release = Arc::new(Mutex::new(false));
        let activator = Activator::new(Arc::new(Recorder {
            started: tx,
            release: Arc::clone(&release),
        }));
        let act = Activation::Systemd("x.service".to_owned());

        activator.try_start("a/b", &act);
        assert_eq!(rx.recv().unwrap(), "a/b", "the first request runs");

        for _ in 0..5 {
            activator.try_start("a/b", &act);
        }
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(200)),
            Err(mpsc::RecvTimeoutError::Timeout),
            "requests while one is in flight must be dropped"
        );

        *release.lock().unwrap() = true;
    }

    #[test]
    fn distinct_activations_do_not_block_each_other() {
        let (tx, rx) = mpsc::channel();
        let release = Arc::new(Mutex::new(false));
        let activator = Activator::new(Arc::new(Recorder {
            started: tx,
            release: Arc::clone(&release),
        }));

        activator.try_start("a/b", &Activation::Systemd("x.service".to_owned()));
        activator.try_start("c/d", &Activation::Systemd("y.service".to_owned()));
        let mut seen = vec![rx.recv().unwrap(), rx.recv().unwrap()];
        seen.sort();
        assert_eq!(seen, vec!["a/b".to_owned(), "c/d".to_owned()]);

        *release.lock().unwrap() = true;
    }

    /// Init runs one service for several interfaces; a second spawn would overwrite the first.
    #[test]
    fn instances_sharing_one_exec_start_it_once() {
        let (tx, rx) = mpsc::channel();
        let release = Arc::new(Mutex::new(false));
        let activator = Activator::new(Arc::new(Recorder {
            started: tx,
            release: Arc::clone(&release),
        }));
        let act = Activation::Exec(vec!["/usr/bin/food".to_owned()]);

        activator.try_start("com.example.IFoo/a", &act);
        assert_eq!(rx.recv().unwrap(), "com.example.IFoo/a");
        activator.try_start("com.example.IFoo/b", &act);
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(200)),
            Err(mpsc::RecvTimeoutError::Timeout),
            "a shared activation already in flight must not run again"
        );

        *release.lock().unwrap() = true;
    }

    /// The `Activator` outlives a reload; a new `start` for a running name must not spawn a second.
    #[test]
    fn a_name_changed_by_reload_while_starting_is_not_started_again() {
        let (tx, rx) = mpsc::channel();
        let release = Arc::new(Mutex::new(false));
        let activator = Activator::new(Arc::new(Recorder {
            started: tx,
            release: Arc::clone(&release),
        }));

        activator.try_start("a/b", &Activation::Exec(vec!["/x".to_owned()]));
        assert_eq!(rx.recv().unwrap(), "a/b");
        activator.try_start(
            "a/b",
            &Activation::Exec(vec!["/x".to_owned(), "-v".to_owned()]),
        );
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(200)),
            Err(mpsc::RecvTimeoutError::Timeout),
            "a name already starting must not start again under a new activation"
        );

        *release.lock().unwrap() = true;
    }

    fn chmod(path: &std::path::Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// `<base>/bin/svc` under a held root, every mode explicit so the umask cannot decide.
    struct Tree {
        base: std::path::PathBuf,
        root: std::os::fd::OwnedFd,
    }

    impl Tree {
        fn new(tag: &str) -> Self {
            let base = std::env::temp_dir().join(format!("rsb-act-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(base.join("bin")).unwrap();
            chmod(&base, 0o755);
            chmod(&base.join("bin"), 0o755);
            std::fs::write(base.join("bin/svc"), "#!/bin/sh\n").unwrap();
            let root = open_root(&base).unwrap();
            Tree { base, root }
        }

        fn command(&self) -> Result<Command, String> {
            let root = At {
                fd: self.root.as_fd(),
                shown: &self.base,
            };
            let argv = ["/bin/svc".to_owned(), "-v".to_owned()];
            exec_command(root, &argv, rustix::process::getuid().as_raw())
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    /// A failed reload keeps the old declaration; the start itself must refuse what load would.
    #[test]
    fn an_exec_start_refuses_a_program_that_fails_the_check_now() {
        let tree = Tree::new("recheck");
        chmod(&tree.base.join("bin/svc"), 0o755);
        let command = tree.command().expect("a trusted program gets a command");
        assert_eq!(command.get_program(), "/bin/svc");
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["-v"]);

        chmod(&tree.base.join("bin"), 0o775);
        let why = tree
            .command()
            .expect_err("a group-writable directory refuses the start");
        let bin = tree.base.join("bin");
        assert!(
            why.contains(&format!("{} is group-writable", bin.display())),
            "{why}"
        );
    }

    /// A started service that kept the hub's socket could report the hub's unit ready or stopping.
    #[test]
    fn an_exec_start_does_not_pass_on_the_notify_socket() {
        let tree = Tree::new("notify");
        chmod(&tree.base.join("bin/svc"), 0o755);
        let command = tree.command().expect("a trusted program gets a command");
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == crate::notify::NOTIFY_SOCKET && value.is_none()),
            "{:?}",
            command.get_envs().collect::<Vec<_>>()
        );
    }

    /// Exec needs `x` only; root reads anything, so the case exists only for another uid.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn an_exec_start_accepts_an_execute_only_program() {
        if rustix::process::geteuid().is_root() {
            eprintln!("skipped: root reads a mode-0111 file, so nothing is tested");
            return;
        }
        let tree = Tree::new("xonly");
        chmod(&tree.base.join("bin/svc"), 0o111);
        assert!(tree.command().is_ok(), "{:?}", tree.command().err());
    }

    struct Panicker(mpsc::Sender<()>);

    impl ActivationRunner for Panicker {
        fn run(&self, _name: &str, _activation: &Activation) {
            self.0.send(()).unwrap();
            panic!("runner failure injected by the test");
        }
    }

    /// Without the drop guard the slot outlives the panic and every later start is dropped.
    #[test]
    fn a_panicking_runner_frees_its_slot() {
        let (tx, rx) = mpsc::channel();
        let activator = Activator::new(Arc::new(Panicker(tx)));
        let act = Activation::Systemd("x.service".to_owned());

        activator.try_start("a/b", &act);
        rx.recv().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !activator.state.lock().unwrap().in_flight.is_empty() {
            assert!(std::time::Instant::now() < deadline, "slot never freed");
            std::thread::sleep(Duration::from_millis(10));
        }
        activator.try_start("a/b", &act);
        rx.recv_timeout(Duration::from_secs(5))
            .expect("the name must start again after a panic");
    }

    /// Ends at once, as a program that exits before it registers; `hold` keeps it running.
    struct Ending {
        started: mpsc::Sender<String>,
        hold: Arc<Mutex<bool>>,
    }

    impl ActivationRunner for Ending {
        fn run(&self, name: &str, _activation: &Activation) {
            self.started.send(name.to_owned()).unwrap();
            while *self.hold.lock().unwrap() {
                std::thread::yield_now();
            }
        }
    }

    /// An activator on a clock the test moves, with what its runner started.
    struct Timed {
        activator: Activator,
        now: Arc<Mutex<Instant>>,
        started: mpsc::Receiver<String>,
        hold: Arc<Mutex<bool>>,
    }

    impl Timed {
        fn new() -> Self {
            let (tx, started) = mpsc::channel();
            let hold = Arc::new(Mutex::new(false));
            let now = Arc::new(Mutex::new(Instant::now()));
            let clock = Arc::clone(&now);
            let activator = Activator::with_clock(
                Arc::new(Ending {
                    started: tx,
                    hold: Arc::clone(&hold),
                }),
                Arc::new(move || *clock.lock().unwrap()),
            );
            Timed {
                activator,
                now,
                started,
                hold,
            }
        }

        fn advance(&self, by: Duration) {
            *self.now.lock().unwrap() += by;
        }

        /// Whether the request ran the runner; returns once its slot is free again.
        fn starts(&self, name: &str, activation: &Activation) -> bool {
            self.activator.try_start(name, activation);
            let ran = self
                .started
                .recv_timeout(Duration::from_millis(200))
                .is_ok();
            self.idle();
            ran
        }

        fn idle(&self) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !self.activator.state.lock().unwrap().in_flight.is_empty() {
                assert!(Instant::now() < deadline, "slot never freed");
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    fn exec(program: &str) -> Activation {
        Activation::Exec(vec![program.to_owned()])
    }

    /// `/usr/bin/false` under a client that polls would otherwise run once a lookup.
    #[test]
    fn an_exec_that_registers_nothing_is_held_off_longer_each_time() {
        let t = Timed::new();
        let act = exec("/usr/bin/false");
        assert!(t.starts("a/b", &act), "the first request runs");
        for secs in [1, 2, 4, 8, 16, 30, 30] {
            assert!(!t.starts("a/b", &act), "held off at once ({secs} s step)");
            t.advance(Duration::from_secs(secs) - Duration::from_millis(1));
            assert!(!t.starts("a/b", &act), "held off 1 ms short of {secs} s");
            t.advance(Duration::from_millis(1));
            assert!(t.starts("a/b", &act), "runs again after {secs} s");
        }
    }

    /// A lazy service exits when its clients go; that is not a failed start.
    #[test]
    fn a_registration_during_the_start_holds_nothing_off() {
        let t = Timed::new();
        let act = exec("/usr/bin/lazyd");
        *t.hold.lock().unwrap() = true;
        t.activator.try_start("a/b", &act);
        assert_eq!(t.started.recv().unwrap(), "a/b");
        t.activator.registered(&act);
        *t.hold.lock().unwrap() = false;
        t.idle();
        assert!(t.starts("a/b", &act), "the next lookup starts it again");
    }

    /// The program was repaired and registered by hand: the count starts over.
    #[test]
    fn a_registration_ends_a_hold_off_and_its_count() {
        let t = Timed::new();
        let act = exec("/usr/bin/false");
        assert!(t.starts("a/b", &act));
        t.advance(Duration::from_secs(1));
        assert!(t.starts("a/b", &act));
        assert!(!t.starts("a/b", &act), "held off for 2 s now");
        t.activator.registered(&act);
        assert!(t.starts("a/b", &act), "a registration ends the hold-off");
        t.advance(Duration::from_secs(1));
        assert!(t.starts("a/b", &act), "and the count: 1 s again, not 4 s");
    }

    /// A reload that changes the argv is another program to judge.
    #[test]
    fn a_hold_off_is_of_one_activation() {
        let t = Timed::new();
        assert!(t.starts("a/b", &exec("/usr/bin/false")));
        assert!(!t.starts("a/b", &exec("/usr/bin/false")));
        assert!(t.starts("a/b", &exec("/usr/bin/true")));
        assert!(t.starts("c/d", &exec("/usr/bin/other")));
    }

    /// `systemctl --no-block start` ends at once every time; the unit has `StartLimit*=`.
    #[test]
    fn a_systemd_start_is_never_held_off() {
        let t = Timed::new();
        let act = Activation::Systemd("x.service".to_owned());
        for _ in 0..3 {
            assert!(t.starts("a/b", &act));
        }
    }
}
