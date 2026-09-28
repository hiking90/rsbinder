// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Readiness notification for a service manager run under systemd.
//!
//! AOSP's `servicemanager` sets the `servicemanager.ready` property once it
//! has become the context manager, and init orders everything that needs
//! binder after it. The portable Linux equivalent is systemd's
//! `Type=notify`: the unit is not "started" until the daemon says so, and
//! units ordered `After=` it are held back until then. Without it a unit
//! file can only use `Type=simple`, which reports success the moment
//! `fork`/`exec` returns — before the binder device is open, before the
//! configuration is validated, and before handle 0 exists. Every service
//! ordered after it would then race the hub.
//!
//! The protocol is small enough not to justify linking `libsystemd`: send
//! one newline-separated `KEY=value` datagram to the `AF_UNIX` socket named
//! by `$NOTIFY_SOCKET`. A leading `@` (or NUL) means an abstract socket.
//! See `sd_notify(3)`.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixDatagram;
use std::path::Path;

/// The environment variable systemd sets for a `Type=notify` service.
const NOTIFY_SOCKET: &str = "NOTIFY_SOCKET";

/// Sends readiness and status to whatever supervises this process — or
/// nothing at all, when nothing does.
///
/// Disabled is the normal case: `rsb_hub` run from a terminal has no
/// `$NOTIFY_SOCKET`, and every method is then a no-op rather than an error.
pub struct Notifier {
    socket: Option<OsString>,
}

impl Notifier {
    /// Take `$NOTIFY_SOCKET` from the environment, **removing it**.
    ///
    /// Removing is what `sd_notify(3)`'s `unset_environment` flag does, and
    /// it matters more here than in a typical daemon: `rsb_hub` starts
    /// declared services itself, and a child that inherited the variable
    /// could report *this* unit ready, or stopping, on its own schedule.
    ///
    /// Call before any other thread exists: it calls `std::env::remove_var`,
    /// which races libc `getenv` in other threads.
    pub fn from_environment() -> Notifier {
        let socket = std::env::var_os(NOTIFY_SOCKET);
        if socket.is_some() {
            std::env::remove_var(NOTIFY_SOCKET);
        }
        Notifier::with_socket(socket)
    }

    /// A notifier aimed at an explicit socket, or at nothing when `None`.
    pub fn with_socket(socket: Option<OsString>) -> Notifier {
        Notifier { socket }
    }

    /// Is anything listening? False when not run under a service manager.
    pub fn is_enabled(&self) -> bool {
        self.socket.is_some()
    }

    /// "Startup finished" — for `rsb_hub`, sent once handle 0 is ours and
    /// not one line earlier.
    pub fn ready(&self, status: &str) {
        self.send(&format!("READY=1\nSTATUS={status}\n"));
    }

    /// "Shutting down on purpose", so the supervisor does not treat the
    /// exit as a failure to restart around.
    pub fn stopping(&self, status: &str) {
        self.send(&format!("STOPPING=1\nSTATUS={status}\n"));
    }

    /// One line of free-form state for `systemctl status`.
    pub fn status(&self, status: &str) {
        self.send(&format!("STATUS={status}\n"));
    }

    /// Best effort: failing to notify must not stop the hub from serving.
    fn send(&self, message: &str) {
        let Some(socket) = self.socket.as_deref() else {
            return;
        };
        if let Err(e) = send_datagram(socket, message.as_bytes()) {
            log::warn!(
                "rsb_hub: could not notify {}={}: {e}",
                NOTIFY_SOCKET,
                Path::new(socket).display()
            );
        }
    }
}

/// `socket` is a path or an abstract name, the two spellings `sd_notify(3)` accepts.
fn send_datagram(socket: &OsStr, message: &[u8]) -> std::io::Result<()> {
    let bytes = socket.as_bytes();
    let sock = UnixDatagram::unbound()?;
    // `@name` (documented) and `\0name` (passed through by systemd) are one abstract socket.
    if matches!(bytes.first(), Some(b'@') | Some(0)) {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            // `SocketAddrExt` lives in a different OS module on each target.
            #[cfg(target_os = "android")]
            use std::os::android::net::SocketAddrExt;
            #[cfg(target_os = "linux")]
            use std::os::linux::net::SocketAddrExt;

            let addr = std::os::unix::net::SocketAddr::from_abstract_name(&bytes[1..])?;
            sock.send_to_addr(message, &addr)?;
            return Ok(());
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "abstract unix sockets exist only on Linux",
            ));
        }
    }
    sock.send_to(message, Path::new(socket))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every non-systemd run, every test included, has no socket.
    #[test]
    fn without_a_socket_everything_is_a_no_op() {
        let n = Notifier::with_socket(None);
        assert!(!n.is_enabled());
        n.ready("ignored");
        n.stopping("ignored");
        n.status("ignored");
    }

    /// systemd parses these bytes, so read them back from a real socket.
    #[test]
    fn ready_sends_the_documented_datagram() {
        let dir = std::env::temp_dir().join(format!("rsb-notify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("notify.sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixDatagram::bind(&path).expect("bind");

        let n = Notifier::with_socket(Some(path.clone().into_os_string()));
        assert!(n.is_enabled());
        n.ready("serving 3 names");

        let mut buf = [0u8; 256];
        let len = listener.recv(&mut buf).expect("recv");
        assert_eq!(
            std::str::from_utf8(&buf[..len]).unwrap(),
            "READY=1\nSTATUS=serving 3 names\n"
        );

        n.stopping("bye");
        let len = listener.recv(&mut buf).expect("recv");
        assert_eq!(
            std::str::from_utf8(&buf[..len]).unwrap(),
            "STOPPING=1\nSTATUS=bye\n"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    /// systemd's socket is usually abstract (`@name`), a branch the path test never reaches.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn an_abstract_socket_name_is_understood() {
        #[cfg(target_os = "android")]
        use std::os::android::net::SocketAddrExt;
        #[cfg(target_os = "linux")]
        use std::os::linux::net::SocketAddrExt;

        let name = format!("rsb-notify-test-{}", std::process::id());
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())
            .expect("abstract name");
        let listener = UnixDatagram::bind_addr(&addr).expect("bind abstract");

        Notifier::with_socket(Some(OsString::from(format!("@{name}")))).ready("abstract");
        let mut buf = [0u8; 128];
        let len = listener.recv(&mut buf).expect("recv");
        assert_eq!(
            std::str::from_utf8(&buf[..len]).unwrap(),
            "READY=1\nSTATUS=abstract\n"
        );
    }

    /// A socket that has gone away must not take the hub down with it.
    #[test]
    fn a_dead_socket_is_survivable() {
        let n = Notifier::with_socket(Some(OsString::from("/nonexistent/rsb-notify.sock")));
        assert!(n.is_enabled());
        n.ready("nobody is listening");
    }
}
