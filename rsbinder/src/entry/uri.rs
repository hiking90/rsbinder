// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Endpoint URI grammar for [`serve`](super::serve) / [`connect`](super::connect).
//!
//! ```text
//! binder://[<service>][?driver=<path>&threads=<n>]
//! unix://<abs-path>[#<service>]               (three slashes: unix:///tmp/x.sock)
//! unix-abstract://<name>[#<service>]
//! vsock://<cid>:<port>[#<service>]            feature rpc-vsock
//! tls://<host>:<port>[#<service>]             feature rpc-tls
//! ```
//!
//! Any RPC scheme accepts `?profile=android13plus[-v<N>]` (the AOSP
//! versioned wire, `N` = max `RPC_WIRE_PROTOCOL_VERSION`, default 2).
//! The service name is the `#fragment` on every scheme; `binder://name`
//! is a shorthand for `binder://#name`. Unknown query keys are rejected.

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
        Some(f) => Some(percent_decode_str(f, uri)?),
        None => None,
    };

    let mut driver = None;
    let mut threads = None;
    let mut wire_max_version = None;
    if let Some(q) = query {
        for kv in q.split('&').filter(|s| !s.is_empty()) {
            let (k, v) = kv
                .split_once('=')
                .ok_or_else(|| bad("query item is not `key=value`", uri))?;
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
                service = Some(authority_path.to_string());
            }
            Endpoint::Kernel { driver, threads }
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
                threads: None
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
                threads: Some(0)
            }
        );
        assert_eq!(p("binder://hello").service.as_deref(), Some("hello"));
        assert_eq!(p("binder://#hello").service.as_deref(), Some("hello"));
        let u = p("binder://svc?driver=/dev/binderfs/x&threads=4");
        assert_eq!(
            u.endpoint,
            Endpoint::Kernel {
                driver: Some("/dev/binderfs/x".into()),
                threads: Some(4)
            }
        );
        assert_eq!(u.service.as_deref(), Some("svc"));
        assert!(parse("binder://a#b").is_err());
        assert!(parse("binder://?profile=android13plus").is_err());
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
        assert!(parse("unix:///a#").is_err());
        assert!(parse("http://x").is_err());
        assert!(parse("nope").is_err());
    }
}
