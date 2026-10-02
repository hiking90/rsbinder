# Reconnecting to a Service

A service can go away: its process crashes or restarts, or — over RPC — its
session ends because one connection failed, a reply deadline expired or the
server closed it. Every proxy to it then returns `DeadObject`, and getting
back means looking the service up again, on a new session for RPC, and
registering your callbacks again. `rsbinder::Reconnecting` does that for one
service.

```rust,ignore
use rsbinder::Reconnecting;

let listener = listener.clone();
let hello = Reconnecting::<dyn IHello>::builder("unix:///run/hello.sock#hello")
    .on_connect(move |conn| {
        // Every (re)connection, before callers get the new proxy.
        conn.proxy().register_listener(&listener)?;
        Ok(())
    })
    .build()?;

let reply = hello.with(|h| h.echo("hi"))?;
```

The URI is any the [entry API](./rpc-transport.md) takes: `binder://name`
for the service manager, or an RPC endpoint with `#name`. Leave the fragment
out to connect to a server's root object — the form a libbinder
`RpcServer::setRootObject` server needs, since only an rsbinder server has a
directory of names.

## What happens when the service goes away

The helper notices, starts a thread of its own, looks the service up again
— backing off from 10 ms to at most 1 s between failed attempts — runs
`on_connect`, and only then hands the new proxy to callers. `generation()`
counts the connections made.

How it notices depends on what the lookup returned:

| Looked-up service | Noticed by |
|---|---|
| a kernel binder proxy | its death notification, at once |
| RPC, the session has incoming connections or a serve loop | its death notification, as the session ends |
| RPC otherwise — the default `unix://…#name` | the next call: before it is sent where the socket reports the close (Unix sockets; TCP on Linux/Android), otherwise after that call fails |
| a service in the same process | never: it cannot die on its own |

It never decides from a status code. The call that ended a session can
return `TimedOut`, and a live server's handler can return `DeadObject`.

## Calls while it reconnects

`with` runs your closure once and returns its result. A failed call is never
sent again: only the service knows whether a call may run twice.

With no connection, `with` returns `DeadObject` — but if a reconnect attempt
is in progress it first waits for it, up to `ReconnectPolicy::call_wait`
(1 s). A server that is already back answers an attempt within milliseconds,
so the call runs on the new connection. Between attempts — the server still
down — calls fail at once instead of each waiting a second. Inside a binder
transaction or inside `on_connect` nothing waits: the reconnect may be
waiting on that very thread.

To wait across attempts, use `wait_connected(timeout)`, or
`connected().await` with `tokio`. `current()` returns the proxy without
waiting, for async code that must not block.

## Starting before the server

`build()` tries once without waiting. It fails only for what another
attempt cannot change — a malformed URI, options that do not fit it, a
missing feature, no binder device for a kernel URI (`NoInit`), a kernel
service of another interface (`BadType`), an RPC name lookup on a server
without a directory of names (`BadType`), a name the service manager
refuses to watch, or
`ConnectError::stop` from `on_connect`. A server that is not up yet is not
an error: the helper connects in the background, so a client may start
first. Set `ClientOptions::timeout` through `options(...)` if the server
could accept a connection and never answer.

## `on_connect`

Register callbacks, subscribe, restore what the server lost. Return
`Err(ConnectError::stop(code))` to give up for good; anything else —
including a `?` on a call that failed — counts as a failed attempt and is
retried. A callback the server makes while the hook runs should use
`conn.proxy()`: the helper hands out the new proxy only after the hook
returns.

## Lifetime

Dropping the `Reconnecting` (or `close()`) closes the current RPC session
and stops any reconnect in progress after its current step, without waiting
for it. A callback object that needs the helper should hold
`helper.downgrade()` — a `WeakReconnecting` — not the helper: a strong
handle there keeps the helper, its session and the callback alive in a
cycle.

The helper does not register services. A service that outlives a restarted
service manager must add itself again; on Android, `init` restarts the
services that depend on `servicemanager`.
