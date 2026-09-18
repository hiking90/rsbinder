// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Endpoint URI grammar for [`serve`](super::serve) / [`connect`](super::connect).
//!
//! ```text
//! binder://[<service>][?driver=<path>&threads=<n>&mmap=<bytes>]
//! unix://<abs-path>[#<service>]               (three slashes: unix:///tmp/x.sock)
//! unix-abstract://<name>[#<service>]
//! vsock://<cid>:<port>[#<service>]            feature rpc-vsock
//! tls://<host>:<port>[#<service>]             feature rpc-tls
//! ```
//!
//! Any RPC scheme accepts `?profile=android13plus[-v<N>]` (the AOSP
//! versioned wire, `N` = max `RPC_WIRE_PROTOCOL_VERSION`, default 2).
//! The service name is the `#fragment` on every scheme; `binder://name`
//! is a shorthand for `binder://#name`. Unknown query keys are rejected,
//! and so is a key given twice.

use std::path::PathBuf;

use crate::error::{Result, StatusCode};

/// Where a [`super::Server`] listens or a [`super::Client`] connects.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Endpoint {
    /// Kernel binder through the system service manager.
    Kernel {
        /// `?driver=` — binder device path; `None` = the default.
        driver: Option<PathBuf>,
        /// `?threads=` — max thread-pool size. `None` (the key absent) is
        /// the only thing that means "the default"; `?threads=0` asks for a
        /// literal zero and gets it. See [`crate::ProcessState::init`].
        threads: Option<u32>,
        /// `?mmap=` — size in bytes of the mapping this process receives
        /// transactions into, the real meaning of the "1 MB binder limit".
        /// `None` = [`ProcessState::default_mmap_size`]. Bytes only, no
        /// `K`/`M` suffix. See
        /// [`ProcessState::init_with_mmap_size`] for the range and what
        /// the driver does with it.
        ///
        /// [`ProcessState::default_mmap_size`]: crate::ProcessState::default_mmap_size
        /// [`ProcessState::init_with_mmap_size`]: crate::ProcessState::init_with_mmap_size
        mmap_size: Option<usize>,
    },
    /// Unix-domain socket at a filesystem path.
    Unix(PathBuf),
    /// Linux/Android abstract Unix-domain socket (percent-decoded bytes).
    UnixAbstract(Vec<u8>),
    /// `AF_VSOCK` `(cid, port)`.
    Vsock(u32, u32),
    /// TCP + TLS `host:port` (TCP is TLS-only in rsbinder).
    Tls(String, u16),
}

impl Endpoint {
    /// Kernel binder (`binder://`) rather than one of the RPC transports.
    pub fn is_kernel(&self) -> bool {
        matches!(self, Endpoint::Kernel { .. })
    }

    /// Whether this transport can carry file descriptors out of band.
    /// Only Unix-domain sockets can (`SCM_RIGHTS`); vsock and TLS cannot,
    /// and on kernel binder fds are native rather than an RPC option.
    /// This is the predicate behind the `fd_modes` / `fd_mode` validation
    /// in [`ServeOptions`](super::ServeOptions) and
    /// [`ClientOptions`](super::ClientOptions).
    pub fn supports_fd_passing(&self) -> bool {
        matches!(self, Endpoint::Unix(_) | Endpoint::UnixAbstract(_))
    }

    /// What this transport can offer **at all**, before anything is
    /// negotiated or opened.
    ///
    /// This is what the transport *family* offers, not what a given
    /// session has, and it is not a bound in one direction: the two bits
    /// that differ in practice differ with opposite polarity.
    ///
    /// - [`FD_PASSING`](crate::TransportCaps::FD_PASSING) is an **upper**
    ///   bound. It is set here for a Unix socket, but a session over one
    ///   carries fds only after it negotiates
    // The target only exists with `rpc`, so only link it then.
    #[cfg_attr(
        feature = "rpc",
        doc = "   [`FileDescriptorTransportMode::Unix`](crate::rpc::FileDescriptorTransportMode) —"
    )]
    #[cfg_attr(
        not(feature = "rpc"),
        doc = "   `FileDescriptorTransportMode::Unix` (`rpc` feature) —"
    )]
    ///   the default is `None`, which carries none.
    /// - [`CALLBACKS`](crate::TransportCaps::CALLBACKS) is a **lower**
    ///   bound. It is never set here for an RPC endpoint, because it
    ///   depends on the client having opened incoming connections — a
    ///   session opened with
    ///   [`ClientOptions::incoming_connections`](super::ClientOptions::incoming_connections)
    ///   `> 0` reports it on both ends although this does not.
    ///
    /// So neither direction makes this a substitute for the session
    /// value: use [`Client::caps`](super::Client::caps) or
    // The target only exists with `rpc`, so only link it then.
    #[cfg_attr(
        feature = "rpc",
        doc = "[`RpcSession::caps`](crate::rpc::RpcSession::caps) for what a live"
    )]
    #[cfg_attr(
        not(feature = "rpc"),
        doc = "`RpcSession::caps` (`rpc` feature) for what a live"
    )]
    /// connection actually has.
    pub fn static_caps(&self) -> crate::TransportCaps {
        use crate::TransportCaps as C;
        match self {
            Endpoint::Kernel { .. } => C::KERNEL,
            Endpoint::Unix(_) | Endpoint::UnixAbstract(_) => {
                C::FD_PASSING | C::TRUSTED_UID | C::SAME_HOST
            }
            Endpoint::Vsock(..) | Endpoint::Tls(..) => C::NONE,
        }
    }
}

/// A parsed endpoint URI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Uri {
    pub endpoint: Endpoint,
    /// `#fragment` (or the `binder://name` shorthand).
    pub service: Option<String>,
    /// `?profile=android13plus[-vN]` → `Some(N)`; absent → r34 wire.
    pub wire_max_version: Option<u32>,
}

fn bad(msg: &str, uri: &str) -> StatusCode {
    log::error!("rsbinder uri: {msg}: {uri:?}");
    StatusCode::BadValue
}

pub fn parse(uri: &str) -> Result<Uri> {
    let (scheme, rest) = uri
        .split_once("://")
        .ok_or_else(|| bad("missing `scheme://`", uri))?;
    let (rest, fragment) = match rest.split_once('#') {
        Some((r, f)) => (r, Some(f)),
        None => (rest, None),
    };
    let (authority_path, query) = match rest.split_once('?') {
        Some((a, q)) => (a, Some(q)),
        None => (rest, None),
    };
    let mut service = match fragment {
        Some("") => return Err(bad("empty `#service`", uri)),
        // A raw `?` here is a query written after the fragment, which would
        // otherwise be swallowed into the service name and never applied.
        Some(f) if f.contains('?') => return Err(bad("`?query` must come before `#service`", uri)),
        Some(f) => Some(percent_decode_str(f, uri)?),
        None => None,
    };

    let mut driver = None;
    let mut threads = None;
    let mut mmap_size = None;
    let mut wire_max_version = None;
    if let Some(q) = query {
        for kv in q.split('&').filter(|s| !s.is_empty()) {
            let (k, v) = kv
                .split_once('=')
                .ok_or_else(|| bad("query item is not `key=value`", uri))?;
            let already_set = match k {
                "driver" => driver.is_some(),
                "threads" => threads.is_some(),
                "mmap" => mmap_size.is_some(),
                "profile" => wire_max_version.is_some(),
                _ => false,
            };
            if already_set {
                return Err(bad(&format!("duplicate query key `{k}`"), uri));
            }
            match k {
                "driver" if scheme == "binder" => {
                    driver = Some(PathBuf::from(percent_decode_str(v, uri)?))
                }
                "threads" if scheme == "binder" => {
                    threads = Some(
                        v.parse()
                            .map_err(|_| bad("`threads` is not a number", uri))?,
                    )
                }
                "mmap" if scheme == "binder" => {
                    mmap_size = Some(
                        v.parse()
                            .map_err(|_| bad("`mmap` is not a number of bytes", uri))?,
                    )
                }
                "profile" if scheme != "binder" => {
                    wire_max_version = Some(match v {
                        "android13plus" => 2,
                        "android13plus-v0" => 0,
                        "android13plus-v1" => 1,
                        "android13plus-v2" => 2,
                        _ => return Err(bad("unknown `profile`", uri)),
                    })
                }
                _ => {
                    return Err(bad(
                        &format!("unknown query key `{k}` for `{scheme}://`"),
                        uri,
                    ))
                }
            }
        }
    }

    let endpoint = match scheme {
        "binder" => {
            if !authority_path.is_empty() {
                if service.is_some() {
                    return Err(bad("both `binder://name` and `#name` given", uri));
                }
                // Same decoding as the `#name` form it is shorthand for.
                service = Some(percent_decode_str(authority_path, uri)?);
            }
            Endpoint::Kernel {
                driver,
                threads,
                mmap_size,
            }
        }
        "unix" => {
            // `unix:///tmp/x.sock` ⇒ authority "" + path "/tmp/x.sock".
            if !authority_path.starts_with('/') {
                return Err(bad("`unix://` needs an absolute path (unix:///path)", uri));
            }
            Endpoint::Unix(PathBuf::from(percent_decode_str(authority_path, uri)?))
        }
        "unix-abstract" => {
            if authority_path.is_empty() {
                return Err(bad("`unix-abstract://` needs a name", uri));
            }
            Endpoint::UnixAbstract(percent_decode(authority_path, uri)?)
        }
        "vsock" => {
            let (cid, port) = authority_path
                .split_once(':')
                .ok_or_else(|| bad("`vsock://` needs `cid:port`", uri))?;
            Endpoint::Vsock(
                cid.parse()
                    .map_err(|_| bad("vsock cid is not a number", uri))?,
                port.parse()
                    .map_err(|_| bad("vsock port is not a number", uri))?,
            )
        }
        "tls" => {
            let (host, port) = authority_path
                .rsplit_once(':')
                .ok_or_else(|| bad("`tls://` needs `host:port`", uri))?;
            if host.is_empty() {
                return Err(bad("`tls://` needs a host", uri));
            }
            Endpoint::Tls(
                host.trim_start_matches('[')
                    .trim_end_matches(']')
                    .to_string(),
                port.parse()
                    .map_err(|_| bad("tls port is not a number", uri))?,
            )
        }
        other => return Err(bad(&format!("unknown scheme `{other}://`"), uri)),
    };
    Ok(Uri {
        endpoint,
        service,
        wire_max_version,
    })
}

fn percent_decode(s: &str, uri: &str) -> Result<Vec<u8>> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = b
                .get(i + 1..i + 3)
                // `from_str_radix` accepts a leading sign, so require two
                // hex digits explicitly (`%+9` is not an escape).
                .filter(|h| h.iter().all(u8::is_ascii_hexdigit))
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or_else(|| bad("bad percent-escape", uri))?;
            out.push(hex);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Ok(out)
}

fn percent_decode_str(s: &str, uri: &str) -> Result<String> {
    String::from_utf8(percent_decode(s, uri)?).map_err(|_| bad("percent-escape is not UTF-8", uri))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Uri {
        parse(s).unwrap_or_else(|e| panic!("{s}: {e:?}"))
    }

    #[test]
    fn kernel_forms() {
        let u = p("binder://");
        assert_eq!(
            u.endpoint,
            Endpoint::Kernel {
                driver: None,
                threads: None,
                mmap_size: None
            }
        );
        assert_eq!(u.service, None);
        // `?threads=0` must survive as `Some(0)`. Collapsing it to `None`
        // would turn "single-threaded, like a service manager" back into
        // "give me the default" — the overload `ProcessState::init` exists
        // to have removed.
        assert_eq!(
            p("binder://?threads=0").endpoint,
            Endpoint::Kernel {
                driver: None,
                threads: Some(0),
                mmap_size: None
            }
        );
        assert_eq!(p("binder://hello").service.as_deref(), Some("hello"));
        assert_eq!(p("binder://#hello").service.as_deref(), Some("hello"));
        let u = p("binder://svc?driver=/dev/binderfs/x&threads=4&mmap=4194304");
        assert_eq!(
            u.endpoint,
            Endpoint::Kernel {
                driver: Some("/dev/binderfs/x".into()),
                threads: Some(4),
                mmap_size: Some(4 * 1024 * 1024)
            }
        );
        assert_eq!(u.service.as_deref(), Some("svc"));
        assert!(parse("binder://a#b").is_err());
        assert!(parse("binder://?profile=android13plus").is_err());
        // Bytes only: a `4M` shorthand would have to be guessed at, and
        // this parser refuses what it does not know rather than guess.
        assert!(parse("binder://?mmap=4M").is_err());
        // Last-value-wins would hand `kernel_init` the 4 KB and silently
        // drop the 4 MB the caller also asked for.
        assert!(parse("binder://?mmap=4194304&mmap=4096").is_err());
        assert!(parse("binder://?threads=1&threads=2").is_err());
        // A query after the fragment would become part of the service
        // name and never be applied.
        assert!(parse("binder://#svc?driver=/dev/x").is_err());
        assert!(parse("unix:///tmp/x.sock#svc?profile=android13plus").is_err());
        assert_eq!(p("binder://#a%3Fb").service.as_deref(), Some("a?b"));
        // The range itself is `ProcessState`'s to judge, at init time —
        // the parser only insists on a number.
        assert_eq!(
            p("binder://?mmap=1").endpoint,
            Endpoint::Kernel {
                driver: None,
                threads: None,
                mmap_size: Some(1)
            }
        );
    }

    #[test]
    fn rpc_forms() {
        let u = p("unix:///tmp/x.sock#hello");
        assert_eq!(u.endpoint, Endpoint::Unix("/tmp/x.sock".into()));
        assert_eq!(u.service.as_deref(), Some("hello"));
        assert_eq!(u.wire_max_version, None);
        assert!(parse("unix://relative/path").is_err());
        let u = p("unix-abstract://rsb%00x?profile=android13plus-v1");
        assert_eq!(u.endpoint, Endpoint::UnixAbstract(b"rsb\0x".to_vec()));
        assert_eq!(u.wire_max_version, Some(1));
        assert_eq!(p("vsock://2:5000#s").endpoint, Endpoint::Vsock(2, 5000));
        assert_eq!(
            p("tls://host.example:9000").endpoint,
            Endpoint::Tls("host.example".into(), 9000)
        );
        assert_eq!(
            p("tls://[::1]:9000").endpoint,
            Endpoint::Tls("::1".into(), 9000)
        );
        assert_eq!(
            p("unix:///a?profile=android13plus").wire_max_version,
            Some(2)
        );
        assert!(parse("unix:///a?profile=nope").is_err());
        assert!(parse("unix:///a?threads=1").is_err());
        // Kernel-only: an RPC transport has no receive mapping, so the
        // key is refused there rather than parsed and dropped.
        assert!(parse("unix:///a?mmap=4194304").is_err());
        assert!(parse("unix:///a#").is_err());
        assert!(parse("http://x").is_err());
        assert!(parse("nope").is_err());
    }
}
