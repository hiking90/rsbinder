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

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use super::declaration::Activation;

/// Runs one activation to completion. Injectable so the debounce can be
/// tested without spawning processes.
pub trait ActivationRunner: Send + Sync {
    /// Bring `name` up. Called on a dedicated thread, so blocking here is
    /// expected — for [`Activation::Exec`] it blocks for the service's
    /// whole lifetime.
    fn run(&self, name: &str, activation: &Activation);
}

/// Starts declared services, at most one attempt at a time per name.
pub struct Activator {
    /// Names with a start attempt outstanding.
    ///
    /// Without this a client polling for a service it cannot reach would
    /// spawn a fresh copy on every lookup — and `wait_for_service` polls.
    /// For an `Exec` activation the entry is held for the service's whole
    /// lifetime, which is the debounce you want: never a second copy of a
    /// service that is already running.
    in_flight: Arc<Mutex<BTreeSet<String>>>,
    runner: Arc<dyn ActivationRunner>,
}

impl Activator {
    /// An activator driving `runner`.
    pub fn new(runner: Arc<dyn ActivationRunner>) -> Self {
        Activator {
            in_flight: Arc::new(Mutex::new(BTreeSet::new())),
            runner,
        }
    }

    /// Ask for `name` to be started, unless an attempt is already
    /// outstanding. Returns immediately; the work happens on its own
    /// thread.
    ///
    /// Nothing here reports success. A start that fails is logged and
    /// forgotten: the caller is a `getService` that has already answered
    /// "not registered", and the client's own wait is what will notice the
    /// service appearing.
    pub fn try_start(&self, name: &str, activation: &Activation) {
        {
            let mut in_flight = self.in_flight.lock().expect("activator lock poisoned");
            if !in_flight.insert(name.to_owned()) {
                log::debug!("start of {name} already in flight");
                return;
            }
        }

        let runner = Arc::clone(&self.runner);
        let activation = activation.clone();
        let owned_name = name.to_owned();
        let in_flight = Arc::clone(&self.in_flight);
        let spawned = std::thread::Builder::new()
            .name("rsb_hub:start".to_owned())
            .spawn(move || {
                log::info!("{owned_name} is not registered; starting it on demand");
                runner.run(&owned_name, &activation);
                in_flight
                    .lock()
                    .expect("activator lock poisoned")
                    .remove(&owned_name);
            });
        if let Err(e) = spawned {
            log::error!("failed to spawn the start thread for {name}: {e}");
            self.in_flight
                .lock()
                .expect("activator lock poisoned")
                .remove(name);
        }
    }
}

/// Runs activations for real.
pub struct SystemRunner;

impl SystemRunner {
    /// Absolute paths only, and never a shell. This runs with rsb_hub's
    /// privileges — often root — so resolving the program through `PATH`
    /// would let anything earlier on that path execute as root, and a shell
    /// would make quoting inside the config file part of the trust
    /// boundary. Same reasoning as rsb_device invoking `/bin/mount`.
    fn systemctl() -> Option<&'static str> {
        ["/usr/bin/systemctl", "/bin/systemctl"]
            .into_iter()
            .find(|p| std::path::Path::new(p).exists())
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
                // `--no-block`: systemd queues the job and returns, so this
                // thread is not held for the unit's whole startup. The
                // client is waiting on a registration notification, not on
                // us.
                let mut c = std::process::Command::new(systemctl);
                c.arg("--no-block").arg("start").arg(unit);
                c
            }
            Activation::Exec(argv) => {
                let mut c = std::process::Command::new(&argv[0]);
                c.args(&argv[1..]);
                c
            }
        };

        // Wait, rather than detach: an unreaped child becomes a zombie, and
        // for `Exec` the wait is also what holds the in-flight slot for as
        // long as the service runs — which is exactly the debounce wanted.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

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
        assert!(
            rx.try_recv().is_err(),
            "requests while one is in flight must be dropped"
        );

        *release.lock().unwrap() = true;
    }

    #[test]
    fn distinct_names_do_not_block_each_other() {
        let (tx, rx) = mpsc::channel();
        let release = Arc::new(Mutex::new(false));
        let activator = Activator::new(Arc::new(Recorder {
            started: tx,
            release: Arc::clone(&release),
        }));
        let act = Activation::Systemd("x.service".to_owned());

        activator.try_start("a/b", &act);
        activator.try_start("c/d", &act);
        let mut seen = vec![rx.recv().unwrap(), rx.recv().unwrap()];
        seen.sort();
        assert_eq!(seen, vec!["a/b".to_owned(), "c/d".to_owned()]);

        *release.lock().unwrap() = true;
    }
}
