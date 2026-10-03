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
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use super::declaration::Activation;
use super::trust::{check_program, open_root, At};

/// Runs one activation to completion. Injectable so the debounce can be
/// tested without spawning processes.
pub trait ActivationRunner: Send + Sync {
    /// Bring `name` up. Called on a dedicated thread, so blocking here is
    /// expected — for [`Activation::Exec`] it blocks for the service's
    /// whole lifetime.
    fn run(&self, name: &str, activation: &Activation);
}

/// Starts declared services, at most one attempt at a time per name and per activation.
pub struct Activator {
    /// Starts outstanding; a match on either half blocks a new start (see [`Self::try_start`]).
    in_flight: Arc<Mutex<Vec<(String, Activation)>>>,
    runner: Arc<dyn ActivationRunner>,
}

impl Activator {
    /// An activator driving `runner`.
    pub fn new(runner: Arc<dyn ActivationRunner>) -> Self {
        Activator {
            in_flight: Arc::new(Mutex::new(Vec::new())),
            runner,
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
    /// Nothing here reports success. A start that fails is logged and
    /// forgotten: the caller is a `getService` that has already answered
    /// "not registered", and the client's own wait is what will notice the
    /// service appearing.
    pub fn try_start(&self, name: &str, activation: &Activation) {
        {
            let mut in_flight = self.in_flight.lock().expect("activator lock poisoned");
            if in_flight.iter().any(|(n, a)| n == name || a == activation) {
                log::debug!(
                    "not starting {name}: a start of it or of its activation is outstanding"
                );
                return;
            }
            in_flight.push((name.to_owned(), activation.clone()));
        }

        let runner = Arc::clone(&self.runner);
        let entry = (name.to_owned(), activation.clone());
        let in_flight = Arc::clone(&self.in_flight);
        let spawned = std::thread::Builder::new()
            .name("rsb_hub:start".to_owned())
            .spawn(move || {
                let slot = InFlightSlot { in_flight, entry };
                let (name, activation) = &slot.entry;
                log::info!("{name} is not registered; starting it on demand");
                runner.run(name, activation);
            });
        if let Err(e) = spawned {
            log::error!("failed to spawn the start thread for {name}: {e}");
            self.in_flight
                .lock()
                .expect("activator lock poisoned")
                .retain(|(n, a)| !(n == name && a == activation));
        }
    }
}

/// Frees a start's in-flight slot on drop, so a panicking runner does not wedge it.
struct InFlightSlot {
    in_flight: Arc<Mutex<Vec<(String, Activation)>>>,
    entry: (String, Activation),
}

impl Drop for InFlightSlot {
    fn drop(&mut self) {
        // Poison-tolerant: a panic here during unwind would abort the whole hub.
        let mut in_flight = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
        in_flight.retain(|e| e != &self.entry);
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
                let mut c = Command::new(systemctl);
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
    let mut c = Command::new(program);
    c.args(&argv[1..]);
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

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
        while !activator.in_flight.lock().unwrap().is_empty() {
            assert!(std::time::Instant::now() < deadline, "slot never freed");
            std::thread::sleep(Duration::from_millis(10));
        }
        activator.try_start("a/b", &act);
        rx.recv_timeout(Duration::from_secs(5))
            .expect("the name must start again after a panic");
    }
}
