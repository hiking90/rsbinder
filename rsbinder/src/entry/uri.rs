// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Where [`serve`](super::serve) listens and [`connect`](super::connect)
//! goes: a [`Uri`] value, built from typed parts or parsed from its
//! string form.
//!
//! ```no_run
//! # use rsbinder::*;
//! # fn demo(path: &std::path::Path) -> Result<()> {
//! // Typed: no escaping, any path the OS accepts.
//! let a = Endpoint::unix(path).with_service("hello");
//! // The same value from a configuration string.
//! let b: Uri = "unix:///tmp/hello.sock#hello".parse()?;
//! # Ok(()) }
//! ```
//!
//! # String form
//!
//! ```text
//! binder://[<service>][?driver=<path>&threads=<n>&mmap=<bytes>]
//! unix://<abs-path>[#<service>]               (three slashes: unix:///tmp/x.sock)
//! unix-abstract://<name>[#<service>]
//! vsock://<cid>:<port>[#<service>]            feature rpc-vsock
//! tls://<host>:<port>[#<service>]             feature rpc-tls, IPv6 host in [ ]
//! ```
//!
//! Any RPC scheme accepts `?profile=android13plus[-v<N>]`
//! ([`WireProfile::Android13Plus`]; `N` = max `RPC_WIRE_PROTOCOL_VERSION`,
//! [`WireVersion::MAX`] when omitted). The service name is the
//! `#fragment` on every scheme; `binder://name` is a shorthand for
//! `binder://#name`. Unknown query keys are rejected, and so is a key
//! given twice. Paths, names, hosts and the service are percent-decoded,
//! so an IPv6 zone id is written `%25` (`tls://[fe80::1%25eth0]:9000`).
//! A client connecting to such a host must also set
//! `ClientOptions::tls_server_name`: the host is the default TLS server
//! name, and a server name cannot hold a zone id, so without it the
//! connect fails with `StatusCode::RpcError`.
//!
//! [`Display`](std::fmt::Display) writes the string form back, escaping
//! what the parser would split on. For every `Uri` that
//! [`serve`](super::serve) / [`connect`](super::connect) accept, parsing
//! that string gives the same value; for one the string form cannot carry
//! (a relative `unix` path, an empty name, host or service, a wire profile
//! on kernel binder), parsing it is an error. A kernel driver path that is
//! not UTF-8 reads back as the same value but is [`StatusCode::BadValue`]
//! at `serve` / `connect`, which open the driver by a `&str`.

use std::ffi::OsString;
use std::fmt::{self, Write as _};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::error::StatusCode;

/// Where a [`super::Server`] listens or a [`super::Client`] connects.
///
/// Build one with [`Endpoint::kernel`], [`Endpoint::unix`],
/// [`Endpoint::unix_abstract`], [`Endpoint::vsock`] or [`Endpoint::tls`],
/// and add a service name with [`Endpoint::with_service`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Endpoint {
    /// Kernel binder through the system service manager.
    Kernel(KernelEndpoint),
    /// Unix-domain socket at an absolute filesystem path.
    Unix(PathBuf),
    /// Linux/Android abstract Unix-domain socket (raw name bytes).
    UnixAbstract(Vec<u8>),
    /// `AF_VSOCK` address.
    Vsock {
        /// Context id of the peer.
        cid: u32,
        /// Port on that context.
        port: u32,
    },
    /// TCP + TLS (TCP is TLS-only in rsbinder).
    Tls {
        /// Host name or IP address literal, without `[ ]`.
        host: String,
        /// TCP port.
        port: u16,
    },
}

impl Endpoint {
    /// Kernel binder with the default driver, thread pool and mapping.
    /// Set those with [`KernelEndpoint`].
    pub fn kernel() -> Self {
        Endpoint::Kernel(KernelEndpoint::default())
    }

    /// Unix-domain socket at `path`, which must be absolute.
    pub fn unix(path: impl Into<PathBuf>) -> Self {
        Endpoint::Unix(path.into())
    }

    /// Abstract Unix-domain socket named `name` (Linux/Android).
    pub fn unix_abstract(name: impl Into<Vec<u8>>) -> Self {
        Endpoint::UnixAbstract(name.into())
    }

    /// `AF_VSOCK` socket at `(cid, port)`.
    pub fn vsock(cid: u32, port: u32) -> Self {
        Endpoint::Vsock { cid, port }
    }

    /// TLS over TCP to `host:port`.
    pub fn tls(host: impl Into<String>, port: u16) -> Self {
        Endpoint::Tls {
            host: host.into(),
            port,
        }
    }

    /// This endpoint with the service named `name`, for
    /// [`connect`](super::connect).
    pub fn with_service(self, name: impl Into<String>) -> Uri {
        Uri::new(self).with_service(name)
    }

    /// Kernel binder (`binder://`) rather than one of the RPC transports.
    pub fn is_kernel(&self) -> bool {
        matches!(self, Endpoint::Kernel(_))
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
            Endpoint::Kernel(_) => C::KERNEL,
            Endpoint::Unix(_) | Endpoint::UnixAbstract(_) => {
                C::FD_PASSING | C::TRUSTED_UID | C::SAME_HOST
            }
            Endpoint::Vsock { .. } | Endpoint::Tls { .. } => C::NONE,
        }
    }
}

impl From<KernelEndpoint> for Endpoint {
    fn from(k: KernelEndpoint) -> Self {
        Endpoint::Kernel(k)
    }
}

/// Kernel binder endpoint: which driver, and how this process sets it up.
///
/// Every setting left unset means the default. The settings apply to the
/// process-wide [`ProcessState`](crate::ProcessState), so
/// [`serve`](super::serve) / [`Client::open`](super::Client::open) refuse
/// one that disagrees with a `ProcessState` already initialized.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct KernelEndpoint {
    pub(crate) driver: Option<PathBuf>,
    pub(crate) threads: Option<u32>,
    pub(crate) mmap_size: Option<usize>,
}

impl KernelEndpoint {
    /// Binder device path (`?driver=`). One that is not UTF-8 is
    /// [`StatusCode::BadValue`] at `serve` / `connect`.
    pub fn with_driver(mut self, path: impl Into<PathBuf>) -> Self {
        self.driver = Some(path.into());
        self
    }

    /// Max thread-pool size (`?threads=`). `0` asks for a literal zero and
    /// gets it; leave it unset for the default. See
    /// [`ProcessState::init`](crate::ProcessState::init).
    pub fn with_threads(mut self, n: u32) -> Self {
        self.threads = Some(n);
        self
    }

    /// Size in bytes of the mapping this process receives transactions
    /// into (`?mmap=`), the real meaning of the "1 MB binder limit". See
    /// [`ProcessState::init_with_mmap_size`](crate::ProcessState::init_with_mmap_size)
    /// for the range and what the driver does with it; that range is
    /// checked there, not here.
    pub fn with_mmap_size(mut self, bytes: usize) -> Self {
        self.mmap_size = Some(bytes);
        self
    }

    /// The driver path, if set.
    pub fn driver(&self) -> Option<&Path> {
        self.driver.as_deref()
    }

    /// The thread-pool size, if set.
    pub fn threads(&self) -> Option<u32> {
        self.threads
    }

    /// The mapping size in bytes, if set.
    pub fn mmap_size(&self) -> Option<usize> {
        self.mmap_size
    }
}

/// Which RPC wire a session speaks. A kernel binder [`Uri`] takes only
/// [`WireProfile::R34`]; any other is `BadValue` at `serve` / `connect`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum WireProfile {
    /// The AOSP android-12 r34 wire, without a handshake.
    #[default]
    R34,
    /// The AOSP android-13+ versioned wire (`?profile=android13plus-vN`),
    /// offering versions up to the given one.
    Android13Plus(WireVersion),
}

/// An android-13+ `RPC_WIRE_PROTOCOL_VERSION` that rsbinder speaks:
/// `0` (android-13), `1` (android-14/15), `2` (android-16).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WireVersion(u32);

impl WireVersion {
    /// android-13.
    pub const V0: Self = WireVersion(0);
    /// android-14 and android-15.
    pub const V1: Self = WireVersion(1);
    /// android-16.
    pub const V2: Self = WireVersion(2);
    /// The highest version rsbinder speaks.
    pub const MAX: Self = Self::V2;

    /// `v` if rsbinder speaks it. The experimental version is not one;
    /// set it on the low-level `RpcServer` / `RpcSession` instead.
    pub fn new(v: u32) -> Result<Self, UriError> {
        if v <= Self::MAX.0 {
            Ok(WireVersion(v))
        } else {
            Err(UriError::new(
                UriErrorKind::UnsupportedWireVersion,
                format!("wire version above {}", Self::MAX.0),
                v.to_string(),
            ))
        }
    }

    /// The version number.
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// A destination: an [`Endpoint`], the service to look up there, and the
/// RPC wire to speak. What [`serve`](super::serve),
/// [`connect`](super::connect) and [`Client::open`](super::Client::open)
/// take.
///
/// Build it from typed parts ([`Uri::new`], [`Uri::kernel`],
/// [`Endpoint::with_service`]) or parse its string form
/// ([`Uri::parse`], [`str::parse`]); see the [module docs](self) for the
/// grammar and for what [`Display`](fmt::Display) writes back.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Uri {
    pub(crate) endpoint: Endpoint,
    pub(crate) service: Option<String>,
    pub(crate) wire: WireProfile,
}

impl Uri {
    /// `endpoint` with no service and the r34 wire.
    pub fn new(endpoint: impl Into<Endpoint>) -> Self {
        Uri {
            endpoint: endpoint.into(),
            service: None,
            wire: WireProfile::R34,
        }
    }

    /// Kernel binder with default settings (`binder://`).
    pub fn kernel() -> Self {
        Uri::new(Endpoint::kernel())
    }

    /// Parse the string form (see the [module docs](self)).
    pub fn parse(s: &str) -> Result<Self, UriError> {
        parse(s)
    }

    /// The service to look up. [`serve`](super::serve) ignores it, so a
    /// server and its clients can share one value; an empty name is
    /// `BadValue` at both `serve` and `connect`.
    pub fn with_service(mut self, name: impl Into<String>) -> Self {
        self.service = Some(name.into());
        self
    }

    /// The RPC wire to speak. Kernel endpoints take only
    /// [`WireProfile::R34`].
    pub fn with_wire(mut self, wire: WireProfile) -> Self {
        self.wire = wire;
        self
    }

    /// Where to listen or connect.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// The service name, if any.
    pub fn service(&self) -> Option<&str> {
        self.service.as_deref()
    }

    /// The RPC wire.
    pub fn wire(&self) -> WireProfile {
        self.wire
    }

    /// Constructors stay infallible; values the string form cannot carry are `BadValue` here.
    pub(crate) fn validate(&self) -> crate::Result<()> {
        let refuse = |what: &str| {
            log::error!("rsbinder: {what}: {self:?}");
            Err(StatusCode::BadValue)
        };
        match &self.endpoint {
            Endpoint::Unix(path) if !path.is_absolute() => {
                return refuse("a `unix` endpoint needs an absolute path")
            }
            Endpoint::UnixAbstract(name) if name.is_empty() => {
                return refuse("a `unix-abstract` endpoint needs a name")
            }
            Endpoint::Tls { host, .. } if host.is_empty() => {
                return refuse("a `tls` endpoint needs a host")
            }
            Endpoint::Kernel(_) if self.wire != WireProfile::R34 => {
                return refuse("kernel binder has no RPC wire profile")
            }
            _ => {}
        }
        if self.service.as_deref() == Some("") {
            return refuse("empty service name");
        }
        Ok(())
    }
}

impl FromStr for Uri {
    type Err = UriError;

    fn from_str(s: &str) -> Result<Self, UriError> {
        parse(s)
    }
}

impl From<Endpoint> for Uri {
    fn from(e: Endpoint) -> Self {
        Uri::new(e)
    }
}

impl From<&Endpoint> for Uri {
    fn from(e: &Endpoint) -> Self {
        Uri::new(e.clone())
    }
}

impl From<KernelEndpoint> for Uri {
    fn from(k: KernelEndpoint) -> Self {
        Uri::new(k)
    }
}

impl From<&Uri> for Uri {
    fn from(u: &Uri) -> Self {
        u.clone()
    }
}

impl fmt::Display for Uri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = String::new();
        let mut query: Vec<(&str, String)> = Vec::new();
        // An empty service in the `binder://name` form would read back as no service.
        let mut service_in_fragment = true;
        match &self.endpoint {
            Endpoint::Kernel(k) => {
                out.push_str("binder://");
                if let Some(s) = self.service.as_deref().filter(|s| !s.is_empty()) {
                    encode(s.as_bytes(), &mut out);
                    service_in_fragment = false;
                }
                if let Some(d) = &k.driver {
                    let mut v = String::new();
                    encode(d.as_os_str().as_bytes(), &mut v);
                    query.push(("driver", v));
                }
                if let Some(n) = k.threads {
                    query.push(("threads", n.to_string()));
                }
                if let Some(n) = k.mmap_size {
                    query.push(("mmap", n.to_string()));
                }
            }
            Endpoint::Unix(path) => {
                out.push_str("unix://");
                encode(path.as_os_str().as_bytes(), &mut out);
            }
            Endpoint::UnixAbstract(name) => {
                out.push_str("unix-abstract://");
                encode(name, &mut out);
            }
            Endpoint::Vsock { cid, port } => {
                let _ = write!(out, "vsock://{cid}:{port}");
            }
            Endpoint::Tls { host, port } => {
                out.push_str("tls://");
                let bracket = host.contains(':');
                if bracket {
                    out.push('[');
                }
                encode(host.as_bytes(), &mut out);
                if bracket {
                    out.push(']');
                }
                let _ = write!(out, ":{port}");
            }
        }
        if let WireProfile::Android13Plus(v) = self.wire {
            // Always the explicit version: the bare form would read back as a later `MAX`.
            query.push(("profile", format!("android13plus-v{}", v.get())));
        }
        for (i, (k, v)) in query.iter().enumerate() {
            out.push(if i == 0 { '?' } else { '&' });
            out.push_str(k);
            out.push('=');
            out.push_str(v);
        }
        if service_in_fragment {
            if let Some(s) = &self.service {
                out.push('#');
                encode(s.as_bytes(), &mut out);
            }
        }
        f.write_str(&out)
    }
}

/// Why a string is not a [`Uri`] (or a number not a [`WireVersion`]).
///
/// Converts into [`StatusCode::BadValue`] for `?` in functions returning
/// [`rsbinder::Result`](crate::Result); that conversion logs the reason,
/// since the status code cannot carry it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UriError {
    kind: UriErrorKind,
    message: String,
    input: String,
}

/// The class of a [`UriError`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum UriErrorKind {
    /// No `scheme://`.
    MissingScheme,
    /// A scheme other than the five in the grammar.
    UnknownScheme,
    /// `unix://` without an absolute path.
    RelativeUnixPath,
    /// `unix-abstract://` without a name.
    EmptyName,
    /// `tls://` without a host.
    EmptyHost,
    /// An empty `#service`.
    EmptyService,
    /// `vsock://` / `tls://` without `:port`.
    MissingPort,
    /// A port, cid, thread count or mapping size that is not a number.
    BadNumber,
    /// A `%` not followed by two hex digits.
    BadPercentEscape,
    /// Decoded bytes that must be UTF-8 (service, host) and are not.
    NotUtf8,
    /// `[` / `]` in a `tls://` host that are not one enclosing pair, or a
    /// `:` in a host without them.
    BadBrackets,
    /// A query item that is not `key=value`.
    MalformedQuery,
    /// A query key the scheme does not take.
    UnknownQueryKey,
    /// A query key given twice.
    DuplicateQueryKey,
    /// `?query` after `#service`.
    QueryAfterService,
    /// A `?profile=` value other than `android13plus[-vN]`.
    UnknownProfile,
    /// A wire version rsbinder does not speak.
    UnsupportedWireVersion,
    /// Both `binder://name` and `#name`.
    ServiceGivenTwice,
}

impl UriError {
    fn new(kind: UriErrorKind, message: impl Into<String>, input: impl Into<String>) -> Self {
        UriError {
            kind,
            message: message.into(),
            input: input.into(),
        }
    }

    /// The class of error.
    pub fn kind(&self) -> UriErrorKind {
        self.kind
    }

    /// The input that failed to parse.
    pub fn input(&self) -> &str {
        &self.input
    }
}

impl fmt::Display for UriError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {:?}", self.message, self.input)
    }
}

impl std::error::Error for UriError {}

impl From<UriError> for StatusCode {
    fn from(e: UriError) -> Self {
        log::error!("rsbinder uri: {e}");
        StatusCode::BadValue
    }
}

fn parse(uri: &str) -> Result<Uri, UriError> {
    use UriErrorKind as K;
    let bad = |kind, msg: &str| UriError::new(kind, msg, uri);
    let (scheme, rest) = uri
        .split_once("://")
        .ok_or_else(|| bad(K::MissingScheme, "missing `scheme://`"))?;
    if !matches!(
        scheme,
        "binder" | "unix" | "unix-abstract" | "vsock" | "tls"
    ) {
        return Err(bad(
            K::UnknownScheme,
            &format!("unknown scheme `{scheme}://`"),
        ));
    }
    let (rest, fragment) = match rest.split_once('#') {
        Some((r, f)) => (r, Some(f)),
        None => (rest, None),
    };
    let (authority_path, query) = match rest.split_once('?') {
        Some((a, q)) => (a, Some(q)),
        None => (rest, None),
    };
    let decode_str = |s: &str| {
        String::from_utf8(percent_decode(s, uri)?)
            .map_err(|_| bad(K::NotUtf8, "percent-escape is not UTF-8"))
    };
    let decode_path =
        |s: &str| Ok::<_, UriError>(PathBuf::from(OsString::from_vec(percent_decode(s, uri)?)));
    let mut service = match fragment {
        Some("") => return Err(bad(K::EmptyService, "empty `#service`")),
        // A raw `?` is a query after the fragment; it would join the service name, never applied.
        Some(f) if f.contains('?') => {
            return Err(bad(
                K::QueryAfterService,
                "`?query` must come before `#service`",
            ))
        }
        Some(f) => Some(decode_str(f)?),
        None => None,
    };

    let mut kernel = KernelEndpoint::default();
    let mut wire = WireProfile::R34;
    let mut seen: Vec<&str> = Vec::new();
    if let Some(q) = query {
        for kv in q.split('&').filter(|s| !s.is_empty()) {
            let (k, v) = kv
                .split_once('=')
                .ok_or_else(|| bad(K::MalformedQuery, "query item is not `key=value`"))?;
            if seen.contains(&k) {
                return Err(bad(
                    K::DuplicateQueryKey,
                    &format!("duplicate query key `{k}`"),
                ));
            }
            seen.push(k);
            match k {
                "driver" if scheme == "binder" => kernel.driver = Some(decode_path(v)?),
                "threads" if scheme == "binder" => {
                    kernel.threads = Some(
                        v.parse()
                            .map_err(|_| bad(K::BadNumber, "`threads` is not a number"))?,
                    )
                }
                "mmap" if scheme == "binder" => {
                    kernel.mmap_size = Some(
                        v.parse()
                            .map_err(|_| bad(K::BadNumber, "`mmap` is not a number of bytes"))?,
                    )
                }
                "profile" if scheme != "binder" => {
                    let version = match v {
                        "android13plus" => WireVersion::MAX,
                        _ => v
                            .strip_prefix("android13plus-v")
                            .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
                            .and_then(|n| n.parse().ok())
                            .ok_or_else(|| bad(K::UnknownProfile, "unknown `profile`"))
                            .and_then(|n| {
                                WireVersion::new(n).map_err(|_| {
                                    bad(K::UnsupportedWireVersion, "unsupported `profile` version")
                                })
                            })?,
                    };
                    wire = WireProfile::Android13Plus(version);
                }
                _ => {
                    return Err(bad(
                        K::UnknownQueryKey,
                        &format!("unknown query key `{k}` for `{scheme}://`"),
                    ))
                }
            }
        }
    }

    let endpoint = match scheme {
        "binder" => {
            if !authority_path.is_empty() {
                if service.is_some() {
                    return Err(bad(
                        K::ServiceGivenTwice,
                        "both `binder://name` and `#name` given",
                    ));
                }
                // Same decoding as the `#name` form it is shorthand for.
                service = Some(decode_str(authority_path)?);
            }
            Endpoint::Kernel(kernel)
        }
        "unix" => {
            // `unix:///tmp/x.sock` ⇒ authority "" + path "/tmp/x.sock".
            if !authority_path.starts_with('/') {
                return Err(bad(
                    K::RelativeUnixPath,
                    "`unix://` needs an absolute path (unix:///path)",
                ));
            }
            Endpoint::Unix(decode_path(authority_path)?)
        }
        "unix-abstract" => {
            if authority_path.is_empty() {
                return Err(bad(K::EmptyName, "`unix-abstract://` needs a name"));
            }
            Endpoint::UnixAbstract(percent_decode(authority_path, uri)?)
        }
        "vsock" => {
            let (cid, port) = authority_path
                .split_once(':')
                .ok_or_else(|| bad(K::MissingPort, "`vsock://` needs `cid:port`"))?;
            Endpoint::Vsock {
                cid: cid
                    .parse()
                    .map_err(|_| bad(K::BadNumber, "vsock cid is not a number"))?,
                port: port
                    .parse()
                    .map_err(|_| bad(K::BadNumber, "vsock port is not a number"))?,
            }
        }
        "tls" => {
            let no_port = || bad(K::MissingPort, "`tls://` needs `host:port`");
            let brackets = || bad(K::BadBrackets, "unbalanced `[ ]` in the `tls://` host");
            let (host, port) = match authority_path.strip_prefix('[') {
                // Cut at `]` first: the last `:` of `[::1]` is inside the literal.
                Some(rest) => {
                    let (h, after) = rest
                        .split_once(']')
                        .filter(|(h, after)| !h.contains('[') && !after.contains(['[', ']']))
                        .ok_or_else(brackets)?;
                    (h, after.strip_prefix(':').ok_or_else(no_port)?)
                }
                None => {
                    let (h, port) = authority_path.rsplit_once(':').ok_or_else(no_port)?;
                    if h.contains(['[', ']']) {
                        return Err(brackets());
                    }
                    if h.contains(':') {
                        return Err(bad(K::BadBrackets, "an IPv6 `tls://` host needs `[ ]`"));
                    }
                    (h, port)
                }
            };
            if host.is_empty() {
                return Err(bad(K::EmptyHost, "`tls://` needs a host"));
            }
            Endpoint::Tls {
                host: decode_str(host)?,
                port: port
                    .parse()
                    .map_err(|_| bad(K::BadNumber, "tls port is not a number"))?,
            }
        }
        other => {
            return Err(bad(
                K::UnknownScheme,
                &format!("unknown scheme `{other}://`"),
            ))
        }
    };
    Ok(Uri {
        endpoint,
        service,
        wire,
    })
}

fn percent_decode(s: &str, uri: &str) -> Result<Vec<u8>, UriError> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = b
                .get(i + 1..i + 3)
                // `from_str_radix` accepts a sign, so require two hex digits (`%+9` is no escape).
                .filter(|h| h.iter().all(u8::is_ascii_hexdigit))
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or_else(|| {
                    UriError::new(UriErrorKind::BadPercentEscape, "bad percent-escape", uri)
                })?;
            out.push(hex);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Ok(out)
}

/// Percent-encode `% # ? & [ ]`, space, ASCII controls and non-UTF-8 bytes; other UTF-8 as is.
fn encode(bytes: &[u8], out: &mut String) {
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            if matches!(c, '%' | '#' | '?' | '&' | '[' | ']' | ' ') || c.is_ascii_control() {
                let _ = write!(out, "%{:02X}", c as u32);
            } else {
                out.push(c);
            }
        }
        for b in chunk.invalid() {
            let _ = write!(out, "%{b:02X}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Uri {
        Uri::parse(s).unwrap_or_else(|e| panic!("{s}: {e:?}"))
    }

    fn kind(s: &str) -> UriErrorKind {
        Uri::parse(s)
            .map(|u| panic!("{s} parsed: {u:?}"))
            .unwrap_err()
            .kind()
    }

    #[test]
    fn kernel_forms() {
        use UriErrorKind as K;
        let u = p("binder://");
        assert_eq!(u.endpoint, Endpoint::kernel());
        assert_eq!(u.service, None);
        // `?threads=0` stays `Some(0)` (single-threaded), distinct from `None` (the default).
        assert_eq!(
            p("binder://?threads=0").endpoint,
            KernelEndpoint::default().with_threads(0).into()
        );
        assert_eq!(p("binder://hello").service(), Some("hello"));
        assert_eq!(p("binder://#hello").service(), Some("hello"));
        let u = p("binder://svc?driver=/dev/binderfs/x&threads=4&mmap=4194304");
        assert_eq!(
            u.endpoint,
            KernelEndpoint::default()
                .with_driver("/dev/binderfs/x")
                .with_threads(4)
                .with_mmap_size(4 * 1024 * 1024)
                .into()
        );
        assert_eq!(u.service(), Some("svc"));
        assert_eq!(kind("binder://a#b"), K::ServiceGivenTwice);
        assert_eq!(kind("binder://?profile=android13plus"), K::UnknownQueryKey);
        // Bytes only: the parser refuses a `4M` shorthand rather than guess its meaning.
        assert_eq!(kind("binder://?mmap=4M"), K::BadNumber);
        // Last-value-wins would silently drop the 4 MB the caller also asked for.
        assert_eq!(
            kind("binder://?mmap=4194304&mmap=4096"),
            K::DuplicateQueryKey
        );
        assert_eq!(kind("binder://?threads=1&threads=2"), K::DuplicateQueryKey);
        // A query after the fragment would join the service name and never be applied.
        assert_eq!(kind("binder://#svc?driver=/dev/x"), K::QueryAfterService);
        assert_eq!(
            kind("unix:///tmp/x.sock#svc?profile=android13plus"),
            K::QueryAfterService
        );
        assert_eq!(p("binder://#a%3Fb").service(), Some("a?b"));
        assert_eq!(kind("binder://?threads"), K::MalformedQuery);
        assert_eq!(kind("binder://#a%zz"), K::BadPercentEscape);
        assert_eq!(kind("binder://#%ff"), K::NotUtf8);
        // The parser only insists on a number; `ProcessState` judges the range at init.
        assert_eq!(
            p("binder://?mmap=1").endpoint,
            KernelEndpoint::default().with_mmap_size(1).into()
        );
    }

    #[test]
    fn rpc_forms() {
        use UriErrorKind as K;
        let u = p("unix:///tmp/x.sock#hello");
        assert_eq!(u.endpoint, Endpoint::unix("/tmp/x.sock"));
        assert_eq!(u.service(), Some("hello"));
        assert_eq!(u.wire, WireProfile::R34);
        assert_eq!(kind("unix://relative/path"), K::RelativeUnixPath);
        let u = p("unix-abstract://rsb%00x?profile=android13plus-v1");
        assert_eq!(u.endpoint, Endpoint::unix_abstract(b"rsb\0x".to_vec()));
        assert_eq!(u.wire, WireProfile::Android13Plus(WireVersion::V1));
        assert_eq!(kind("unix-abstract://"), K::EmptyName);
        assert_eq!(p("vsock://2:5000#s").endpoint, Endpoint::vsock(2, 5000));
        assert_eq!(kind("vsock://2"), K::MissingPort);
        assert_eq!(
            p("tls://host.example:9000").endpoint,
            Endpoint::tls("host.example", 9000)
        );
        assert_eq!(p("tls://[::1]:9000").endpoint, Endpoint::tls("::1", 9000));
        assert_eq!(kind("tls://[[::1]]:9000"), K::BadBrackets);
        assert_eq!(kind("tls://[::1:9000"), K::BadBrackets);
        assert_eq!(kind("tls://a]:9000"), K::BadBrackets);
        assert_eq!(kind("tls://[::1]"), K::MissingPort);
        assert_eq!(kind("tls://[::1]9000"), K::MissingPort);
        assert_eq!(kind("tls://[]:9000"), K::EmptyHost);
        assert_eq!(kind("tls://:9000"), K::EmptyHost);
        assert_eq!(kind("tls://fe80::1"), K::BadBrackets);
        assert_eq!(kind("tls://::1:9000"), K::BadBrackets);
        assert_eq!(kind("tls://h:x"), K::BadNumber);
        assert_eq!(
            p("unix:///a?profile=android13plus").wire,
            WireProfile::Android13Plus(WireVersion::MAX)
        );
        assert_eq!(kind("unix:///a?profile=nope"), K::UnknownProfile);
        assert_eq!(
            kind("unix:///a?profile=android13plus-v9"),
            K::UnsupportedWireVersion
        );
        assert_eq!(kind("unix:///a?threads=1"), K::UnknownQueryKey);
        // Kernel-only: RPC has no receive mapping, so the key is refused, not silently dropped.
        assert_eq!(kind("unix:///a?mmap=4194304"), K::UnknownQueryKey);
        assert_eq!(kind("unix:///a#"), K::EmptyService);
        assert_eq!(kind("http://x"), K::UnknownScheme);
        assert_eq!(kind("http://x?threads=1"), K::UnknownScheme);
        assert_eq!(kind("foo://x?profile=nope"), K::UnknownScheme);
        assert_eq!(kind("foo://x#a?b"), K::UnknownScheme);
        assert_eq!(kind("nope"), K::MissingScheme);
    }

    #[test]
    fn wire_version_range() {
        for v in 0..=WireVersion::MAX.get() {
            assert_eq!(WireVersion::new(v).map(WireVersion::get), Ok(v));
        }
        let e = WireVersion::new(WireVersion::MAX.get() + 1).unwrap_err();
        assert_eq!(e.kind(), UriErrorKind::UnsupportedWireVersion);
        // The experimental version is a low-level `RpcServer` setting, not a `Uri` value.
        assert!(WireVersion::new(0xF000_0000).is_err());
    }

    #[track_caller]
    fn round_trip(u: &Uri) {
        assert_eq!(u.validate(), Ok(()), "{u:?}");
        let s = u.to_string();
        assert_eq!(Uri::parse(&s).as_ref(), Ok(u), "{s}");
    }

    #[track_caller]
    fn refused(u: &Uri) {
        assert_eq!(u.validate(), Err(StatusCode::BadValue), "{u:?}");
        let s = u.to_string();
        assert!(Uri::parse(&s).is_err(), "{u:?} wrote {s}, which parses");
    }

    /// Every byte in every free-text field round-trips through `Display` and `parse`.
    #[test]
    fn display_round_trips_every_byte() {
        for b in 0..=u8::MAX {
            let text = char::from(b).to_string(); // U+0000..U+00FF: one or two UTF-8 bytes
            round_trip(&Uri::new(Endpoint::unix(OsString::from_vec(vec![
                b'/', b'p', b, b'x',
            ]))));
            round_trip(&Uri::new(Endpoint::unix_abstract(vec![b'n', b])));
            round_trip(&Uri::new(
                KernelEndpoint::default().with_driver(OsString::from_vec(vec![b'/', b])),
            ));
            round_trip(&Uri::kernel().with_service(format!("s{text}")));
            round_trip(&Uri::kernel().with_service(format!("{text}s")));
            round_trip(&Endpoint::unix("/x").with_service(format!("s{text}")));
            round_trip(&Uri::new(Endpoint::tls(format!("h{text}"), 443)));
            round_trip(&Uri::new(Endpoint::tls(format!("h:{text}"), 443)));
        }
    }

    #[test]
    fn display_round_trips_every_shape() {
        round_trip(&Uri::kernel());
        round_trip(&Uri::kernel().with_service("hello"));
        round_trip(&Uri::new(
            KernelEndpoint::default()
                .with_driver("/dev/binderfs/x")
                .with_threads(0)
                .with_mmap_size(4096),
        ));
        round_trip(
            &Uri::new(KernelEndpoint::default().with_threads(4)).with_service("a/b.IFoo/default"),
        );
        round_trip(&Uri::new(Endpoint::unix("/tmp/x.sock")));
        round_trip(&Uri::new(Endpoint::unix(OsString::from_vec(
            b"/tmp/rsb-\xff.sock".to_vec(),
        ))));
        round_trip(&Uri::new(Endpoint::unix("/tmp/한글.sock")).with_service("서비스"));
        round_trip(&Uri::new(Endpoint::unix_abstract(b"\0\xff".to_vec())));
        round_trip(&Uri::new(Endpoint::vsock(u32::MAX, 0)));
        round_trip(&Uri::new(Endpoint::tls("::1", 1)));
        round_trip(&Uri::new(Endpoint::tls("a:]b", 1)));
        for v in [
            WireVersion::V0,
            WireVersion::V1,
            WireVersion::V2,
            WireVersion::MAX,
        ] {
            for e in [
                Endpoint::unix("/a"),
                Endpoint::unix_abstract("n"),
                Endpoint::vsock(2, 3),
                Endpoint::tls("h", 4),
            ] {
                round_trip(&e.with_service("s").with_wire(WireProfile::Android13Plus(v)));
            }
        }
    }

    #[test]
    fn refused_values_do_not_read_back() {
        refused(&Uri::new(Endpoint::unix("rel/x")));
        refused(&Uri::new(Endpoint::unix("")));
        refused(&Uri::new(Endpoint::unix_abstract(Vec::new())));
        refused(&Uri::new(Endpoint::tls("", 443)));
        refused(&Uri::kernel().with_service(""));
        refused(&Endpoint::unix("/a").with_service(""));
        refused(&Uri::kernel().with_wire(WireProfile::Android13Plus(WireVersion::V2)));
    }

    #[test]
    fn display_is_the_documented_form() {
        assert_eq!(
            Uri::kernel().with_service("hello").to_string(),
            "binder://hello"
        );
        assert_eq!(
            Endpoint::unix("/tmp/a b#c.sock")
                .with_service("s?")
                .with_wire(WireProfile::Android13Plus(WireVersion::V2))
                .to_string(),
            "unix:///tmp/a%20b%23c.sock?profile=android13plus-v2#s%3F"
        );
        assert_eq!(
            Endpoint::tls("::1", 9000).with_service("s").to_string(),
            "tls://[::1]:9000#s"
        );
    }
}
