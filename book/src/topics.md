# Topics (one publisher, many subscribers)

**Experimental.** The `rsbinder-topic` crate carries records from one
publisher to any number of subscribers over kernel binder, or
[over a Unix domain socket](#topics-without-a-binder-device) where there is no
binder device. A binder call sets each subscription up; after it the records
travel through shared memory, and no binder transaction is made for them:

- The publisher allocates the topic's memory, a ring of records in a sealed
  memfd, and every subscriber maps it read-only. One copy into the ring serves
  all of them.
- A publish does not wait for a subscriber. When the ring is full the oldest
  records are overwritten, and a subscriber that had not read them is told how
  many it lost.
- A subscriber writes one small memfd of its own, its control page, which the
  publisher maps read-only. Nothing a subscriber does reaches the ring or
  another subscriber.

The crate is separate from `rsbinder` and sits in the Experimental
[stability tier](./stability-tiers.md): its API and its shared-memory layout
may change in any release. A publisher and a subscriber of different layout
versions refuse each other (`ITopic::ERROR_VERSION`).

## Topic, stream or callback

| | Topic | [Stream](./streaming.md) | `oneway` callback |
|---|---|---|---|
| Receivers | any number | one | one per registration |
| Who allocates the shared memory | the publisher, once for all | the consumer | none: every value is a transaction |
| A receiver that falls behind | loses the oldest records (`RecvError::Lagged`), or with backpressure holds the publisher up to a timeout and is then taken out | holds the sender back; nothing is lost | calls queue in the receiver's async buffer, and fail once it is full |
| Transports | kernel binder, and RPC on a Unix socket | kernel binder and RPC | kernel binder and RPC |

A topic suits values that many processes read and where the newest matters
more than every one: sensor readings, frames, state that keeps changing. A
receiver that must see every value however slow it is uses a stream. A value
sent now and then, to one process, is a `oneway` method.

## A topic through the service manager

Add `rsbinder-topic` beside `rsbinder`, at the same version. The example below
defines its value type with `#[derive(rsbinder::Parcelable)]`, which needs
`rsbinder`'s `macros` feature:

```toml
[dependencies]
rsbinder = { version = "…", features = ["macros"] }
rsbinder-topic = "…"
```

A topic carries values of one type, a `Payload`: any parcelable, derived or
generated from `.aidl`, or `Bytes`. The two ends compare the type's name when
a subscription is set up; for a derived parcelable that is the descriptor, so
it names the package:

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_readings.rs:payload}}
```

Both processes run the binder thread pool (see [Constraints](#constraints)):

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_readings.rs:init}}
```

The publisher makes the topic, registers its binder, and publishes. Any
thread may publish: `publish` takes `&self`.

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_readings.rs:publish}}
```

A subscriber takes a subscription with one twoway call, `Subscriber::request`.
After it, receiving makes no binder call. `recv` waits for a record; every
error but `Closed` leaves the subscription as it was:

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_readings.rs:subscribe}}
```

The example runs as two processes, with `rsb_hub` running:

```text
cargo run -p rsbinder-topic --example topic_readings -- publish
cargo run -p rsbinder-topic --example topic_readings -- subscribe
```

A subscriber starts at the first record published after its subscription
(`Start::Latest`), so it misses what came before. `SubscriberPolicy::start`
also offers `Start::Last`, whose first receive returns the newest record, and
`Start::Oldest`, which reads everything the ring still holds.

`PublisherPolicy::ring_bytes` sizes the ring (64 KiB by default). A record is
at most half the ring, and how far a subscriber can fall behind before it
loses records is the ring's size over a record's. `PublisherPolicy::for_payload`
works the size out from the largest value and the number of records to keep.

### Handing a topic out from an interface

A topic does not have to be a service of its own. A method of another
interface can return it as an `IBinder`, and the client passes that binder to
`Subscriber::request`:

```aidl
interface ISensors {
    IBinder readings();
}
```

The application's `.aidl` does not import `rsbinder-topic`'s, and must not
copy it into its own AIDL build: the copy generates types of the same names
that `rsbinder-topic` does not take.

### Who may subscribe

`PublisherPolicy::authorize` takes a check on the caller of the subscription
call (`rsbinder::CallingContext`: uid, pid, SELinux context); a refused caller
gets `EX_SECURITY`. `max_subscribers` (64 by default) and `max_per_uid` bound
how many subscriptions the topic holds. An application that wants more than a
predicate implements `ITopic` itself and hands each admitted request to an
`Acceptor` (see the `Acceptor` rustdoc).

## Finding a topic by name

`Publisher::advertise` makes the topic and registers it under a name, and a
`NamedSubscriber` watches the name and connects to whichever publisher holds
it, the two started in either order:

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_named.rs:advertise}}
```

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_named.rs:named}}
```

Where a `Subscriber`'s topic ends with `RecvError::Closed`, a
`NamedSubscriber` returns `RecvError::Disconnected` once (the publisher ended
or died, or another took the name) and waits for the next publisher. Records
do not carry over: what was published between two connections is not counted
as lost, and the `Disconnected` marks the gap. Two things end it, each then
returned by every receive: being taken out by a topic with backpressure
(`Closed::Evicted`), and, once no connection is left, the death of a
registry's host (`Closed::ResolverDied`; the service manager's death is not
reported). `NamedSubscriber::state` tells a wait for a publisher from a wait
only a new registration of the name ends. The watch uses the service
manager's registration notices, which Android 10's service manager does not
have: there `NamedSubscriber::subscribe` fails. A notice is a oneway call,
which the kernel fails when the subscriber's process has no room left for one;
when a publisher ends, or refuses as ended, with no other binder for the name,
the subscriber asks the service manager (`check_service`) for the binder the
name has. It asks once more only when a notice that came during the call gave
the name a binder it cannot use, one it refused or that ended.

Subscribing by name does not start a publisher, except where a lookup of the
service manager does:

| How a subscriber finds it | Starts the publisher? |
|---|---|
| `wait_for_interface` and the like (`getService`), then `Subscriber::request` | yes: [on-demand start](./service-manager.md#starting-a-service-on-demand) |
| `NamedSubscriber` through the service manager | `rsb_hub`: no, it uses `check_service` and registration notices; on Android 15 from `android-15.0.0_r6`, `check_service` is `getService` and can start a declared service |
| `NamedSubscriber` through a kernel registry | no: `lookup` and `watch` start nothing |
| `NamedSubscriber` through a hub on a socket | no, for the same reason; that hub starts no service at all |
| `uds::subscribe` | no: a publisher has to be at the endpoint |

## A registry of topic names

Through the service manager, a topic's name is a service name: `[[rule]]`
decides who may register and find it, it is listed among the services, and it
stays registered until the publisher's process ends. Built with its `topic`
feature, `rsb_hub` also serves a registry of topic names under
`rsbinder.topic.registry`, apart from the service names (its configuration is
in [Service Manager](./service-manager.md#topic-names)):

- Its own rules say who may advertise a name, who may subscribe to it (look
  it up or watch it), and who may list the names.
- A name is taken back when its publisher closes or drops, or its process
  dies; a watch goes when the watcher's process dies.
- `rsb_service topics [PREFIX]`, built with the same feature, lists the
  names with the uid and pid that advertised them, their age and their
  watchers.

`RegistryResolver` is that registry as a `Resolver`, which
`Publisher::advertise_with` and `NamedSubscriber::subscribe_with` take where
`advertise` and `subscribe` use the service manager:

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_registry.rs:find}}
```

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_registry.rs:advertise}}
```

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_registry.rs:subscribe}}
```

`RegistryResolver::find` fails with `NameNotFound` when the service manager
has nothing under `rsbinder.topic.registry` or its policy does not let the
caller find that name; the two cannot be told apart, so a hub with `[[topic]]`
rules whose `[[rule]]` lets nobody find the registry logs a warning as it loads.
`list` returns the
names under a prefix that the caller may subscribe to, sorted:

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_registry.rs:list}}
```

The example's documentation shows how to run it against a hub built with the
feature.

- **One topic per name.** The uid that advertised a name may give it to
  another topic, as a publisher started again does. Another uid is refused
  with `ITopicRegistry::ERROR_TAKEN` while the topic lives, unless the hub
  runs with `--allow-cross-uid-overwrite`. A name is 1 to 127 bytes of
  `[A-Za-z0-9._/-]`, as a service name.
- **Refusals.** An advertise or watch the policy refuses is `EX_SECURITY`
  (so `NamedSubscriber::subscribe_with` fails), as is a `list` by a caller
  `[global] topics` leaves out. A lookup it refuses answers no topic, as for
  a name that has none, and `list` leaves out the names the caller may not
  subscribe to.
- **Notices.** A watch is told of each topic its name is given by a oneway
  call, which the kernel fails when the watcher's process has no room left
  for one. The registry then sends it again, with the topic the name has by
  then: once a second at first, at most once in 30 seconds after a few
  failures, for up to 256 watches at a time. A `NamedSubscriber` also looks
  the name up when its publisher ends, or refuses it as ended, as it does
  through the service manager.
- **Reloads.** A reload of the hub's policy applies to every later call and
  to every notice: a watch whose caller lost `subscribe` is told nothing, and
  hears of the next topic once a reload gives the permission back. A
  subscription already set up is not cut: who stays subscribed is the
  publisher's `authorize`.
- **A stopped publisher.** Receiving a topic, from a lookup or a notice,
  sends nothing to the topic's process, so a lookup of a stopped publisher's
  name answers as any other does. Subscribing is a call to the publisher
  (`ITopic.subscribe`), and it waits for that process while it is stopped.
  The hub waits on nobody to hand a topic out (see
  [Service Manager](./service-manager.md#what-a-caller-can-hold-the-hub-up-with)).

What the registry's check vouches for is that a uid the policy allows chose
the binder a name gives. It does not vouch for the process that writes the
ring: an allowed process can advertise, under a name of its own, a topic it
looked up that another uid publishes. Nor does it hold unless the registry is
the hub's own, which a hub with the feature makes sure of by refusing the
registry's name to every `add`. Weigh both before a subscriber that found its
topic by name calls the `unsafe` `Subscriber::trust_publisher`.

### When the hub restarts

The registry lives in `rsb_hub`, and its names and watches end with it. Every
call of a `RegistryResolver` then fails with `DeadObject`. Publishers and
subscribers already connected go on, but a publisher does not advertise its
name to the next hub, and a `NamedSubscriber` watches nothing: make the
`Publisher` and the `NamedSubscriber` again with a resolver of the new hub.
The service manager path is the same, as its registrations and watches end
with the hub too.

## Topics without a binder device

A binder call sets a subscription up and a binder death link tells each end of
the other's end; the records never go through binder. With the feature `uds`,
those two go over rsbinder's [RPC](./rpc-transport.md) on a Unix domain socket
instead, for a Linux where binderfs cannot be used: a container without
`CAP_SYS_ADMIN`, a kernel without `CONFIG_ANDROID_BINDERFS`, a CI runner with
no binder device. It is not for macOS, which has no `F_SEAL_FUTURE_WRITE` and
no futex, nor for Android yet, where how SELinux judges these sockets has not
been measured.

The plain form has no names. The publisher serves on a socket, and a subscriber
gives the path; nothing else about a topic changes:

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_uds.rs:serve}}
```

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_uds.rs:subscribe}}
```

`uds::Endpoint` is a path, a Linux abstract socket or a descriptor. An abstract
socket has no file mode, so any process of the network namespace can connect to
it: prefer a path, whose mode bits and directories say who may, unless the
topic's `authorize` checks its callers. `Options::expect_uid` refuses a
publisher of any other uid, by `SO_PEERCRED` of the process that called
`listen`. Kernel binder has no such check, and at a socket path it is the one
way for a subscriber to know who listens there.

A session of your own also works: `Subscriber::request` takes the root binder of
a session opened with an incoming connection and `FileDescriptorTransportMode::Unix`,
and refuses what cannot work: `InvalidOperation` for a session with no incoming
connection, on which it would not hear of its publisher's death, and
`FdsNotAllowed` for one that passes no fds, TCP and vsock among them. Keep such
a session open for as long as the subscriber.

### Names on a socket

`rsb_hub --topic-socket PATH`, built with the features `topic` and `rpc`, serves
the registry of topic names on a Unix socket and opens no binder device. Its
`[[topic]]` and `[global] topics` rules are the same ones
([Service Manager](./service-manager.md#a-hub-on-a-unix-socket)), judged by the
caller's `SO_PEERCRED`. `UdsResolver` is that registry as a `Resolver`, with
`advertise_with` and `subscribe_with` as above:

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_uds.rs:hub}}
```

A binder cannot leave the RPC session it arrived on, so this registry hands out
descriptors rather than binders. For each name the hub makes a socket in a
private directory, binds it and unlinks its file. A publisher is given the bound
socket and listens on it; a subscriber is given an `O_PATH` descriptor of the
file and connects through `/proc/self/fd/<n>`. So:

- **The policy says who can reach a topic.** The socket has no name in the file
  system: only a process the hub gave a descriptor to can connect. A process can
  pass its descriptor to another, as it can a kernel binder handle.
- **A subscriber knows its publisher.** The publisher calls `listen`, so
  `SO_PEERCRED` of a subscriber's connection names the publisher and not the hub;
  `ResolverOptions::expect_publisher_uid` is a second check on it, for a hub that
  was set up wrong.
- **A publisher that ends takes its socket with it.** The hub keeps no copy, and
  a connect through a descriptor kept since is `ECONNREFUSED`.

A name lasts until it is withdrawn, or until the `UdsResolver` that advertised
it and all it made have dropped, which closes the session with the hub; a
process that ends does both. `Publisher` holds one advertisement, so one
publisher is not on a kernel registry and on a socket hub at once. `advertise_on`
with any resolver, `ServiceManager` included, replaces the advertisement it has;
`hub::add_service` of its binder takes none and goes with either.
`rsb_service topics --topic-socket PATH` lists the names. When the hub ends, its
names and watches end with it, as [a kernel registry's do](#when-the-hub-restarts).

### What differs from kernel binder

- **The caller's identity is fixed at connect.** Kernel binder names the caller
  of each transaction. Here `Authorize` and `SubscriberInfo` see `SO_PEERCRED`
  of the connection: a process that hands its connected socket to another makes
  that one's calls look like its own.
- **Resources grow per subscriber.** Kernel binder serves every peer with one
  descriptor and one thread pool. Here each subscriber's session is two sockets
  and two threads at each end, for as long as it lasts. The bound of
  `RpcServer::set_max_connections`, and of the server of a `UdsResolver` given
  `ResolverOptions::publisher_connections`, counts serve threads: one per
  subscriber, plus one free while a subscriber's second socket attaches. A
  process using a hub has one more session with it.
- **Sessions outlive what ended them.** On a hub, a dropped `Publisher` withdraws
  its name and refuses new connections, and leaves the sessions it has, so that
  their subscribers read its end record. Each closes as its subscriber ends, or
  with the publisher's process. In the plain form the server is yours: the handle
  of `uds::serve` ends every session as it drops, so end the `Publisher` before the
  handle, or its subscribers read `Closed::PublisherDied`.
- **Death is the session's end.** The loss of any connection of a session is a
  death, and a deadline that runs out on a session ends it too, which reads as
  `Closed::PublisherDied`. The sessions topics make have a deadline while they
  are set up (`Options::timeout`, 10 s) and none afterwards; one you give
  `Subscriber::request` keeps what you set. A `NamedSubscriber`'s subscription call
  has no deadline, so that a publisher that stopped in it is subscribed to once it
  goes on; a new publisher of the name ends the wait. A hub keeps a 10 s deadline
  on what it sends: a process that stopped with its socket full loses its session
  with the hub, and the names and watches on it. A process that reads slowly keeps
  its session and skips generations of a name; reading slowly means taking a whole
  frame within the deadline, as a part of one does not show. No watcher holds up a
  change or another watcher: a send takes one system call, and a notice with no room
  is owed and sent again with the name's topic by then, as on a binder device, under
  one bound on how many are owed. A uid allowed to `subscribe` that stops reading can
  take that room, and a notice past it waits for the name's next change: the hub logs
  a warning, and the count is `owed_dropped` of `UdsRegistry::stats`, which `rsb_hub`
  on a socket does not show. Such a uid with enough stopped sessions can also pass the
  kernel's count of descriptors in flight
  ([Service Manager](./service-manager.md#a-hub-on-a-unix-socket)), and then every
  notice has no room until they are read. A dropped subscriber's session closes, which
  ends its subscription call.
- **A publisher that stops is tried again.** A `NamedSubscriber` that cannot connect
  to the publisher a name has connects again, once a second at first and at most
  once in 30 seconds, until it can or the name has another publisher. A publisher
  of another uid than `expect_publisher_uid` and a socket nobody listens on any more
  are not tried again: the next publisher of the name ends that wait. The first is
  told as a publisher's refusal is, by `Disconnected::Rejected` with `EX_SECURITY`
  and `NamedState::Refused`.
- **A full accept queue is out of reach of a deadline.** Linux has a connect into
  such a listener wait in `connect(2)` until the publisher accepts or ends. A
  stopped publisher with room in its queue is found out by the handshake instead.
- **`uid` and `pid` depend on the namespaces.** Across user namespaces a uid with
  no mapping reads 65534 to `Authorize`, `Options::expect_uid` and a hub's policy,
  and a pid outside this pid namespace reads -1 (`PeerIdentity::pid()` returns
  `None`).
- **`/proc` has to be mounted** for a hub and for `Endpoint::Fd`: a sandbox that
  hides it can use only the plain form, with a path or an abstract name.

## Losing records, and backpressure

Without backpressure a publish never waits. A subscriber that falls more than
a ring behind gets `RecvError::Lagged(n)` for the `n` records written over and
then the next record. `try_latest` and `recv_latest` skip to the newest
record and report nothing skipped as lost.

`PublisherPolicy::backpressure` makes a publish wait, up to
`Backpressure::publish_timeout`, for its subscribers to read what it would
overwrite. Past the timeout the subscribers in the way are taken out: each is
told, and its receives return `Closed::Evicted`. `Backpressure::window` also
bounds how many records a subscriber may be behind. The same timeout bounds a
loan's wait for a slot and the end record's wait in `close` or the drop.

The timeout is the publisher's, not a subscriber's: a subscriber that stops
reading is taken out only once it holds up a publish, and the time until then
is the time the publisher takes to fill the ring (or the window) behind it,
which the publish rate decides, plus the timeout. The wait holds the topic's
lock and sleeps the publishing thread, so from async code a publish runs on a
blocking thread. The `Backpressure` rustdoc lists what waits with it.

## Waiting

| Call | No record unread |
|---|---|
| `try_recv`, `try_latest` | `RecvError::Empty` at once |
| `recv`, `recv_latest` | waits for one |
| `recv_timeout` | waits, then `RecvError::Timeout` |
| `recv_async` (`tokio` feature) | on the blocking pool; with `Wake::Fd`, the reactor |
| `Wait`, `wait_any` (`Wake::Fd`) | the caller polls |

A waiting subscriber first watches the ring for up to 20 µs, then sleeps. By
default (`Wake::Futex`) it sleeps on a word of the topic's memory, and a
publish wakes every such subscriber with one system call, and makes none when
none sleeps. A subscriber made with `Wake::Fd` is woken through a socket of
its own, one system call per waiting subscriber, and has a file descriptor to
poll: one thread waits for several subscriptions with `wait_any` or an event
loop, and `recv_async` holds no thread while it waits. An `Interrupter` ends a
wait from another thread.

Each futex subscriber sleeps on one of 32 futex bits, chosen when it is made.
A wake this process sends for it (an interrupt, the publisher's death, a
dropped `recv_async`) names that bit alone, so it also wakes the other
subscribers of the topic that hold the same bit, about one in 32, and not
every one; those look at the ring and sleep again. A publish wakes every bit.

## Large values: loans and samples

A value too large to copy twice goes through a pool. With
`PublisherPolicy::pool` the publisher writes a value straight into a slot of a
second memfd, and the ring carries only a record that names the slot:

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_frames.rs:lend}}
```

A loan (`LoanMut`) is `Send`, so a spawned task can hold one across an
`.await` and publish it from whichever worker thread it resumes on. With
backpressure, `loan_slice` waits for a free slot and `publish` for room in
the ring, under the topic's lock as a publish does, up to the timeout; both
block that worker thread meanwhile. A `loan_slice` while
`PoolPolicy::max_loans` loans are held is refused with
`PublishError::TooManyLoans` at once, without that wait.

A `Bytes` subscriber receives a lent value as any other, or where it lies with
`recv_sample`, `try_recv_sample`, `try_latest_sample` and `recv_sample_async`;
a `NamedSubscriber` of `Bytes` has the same calls, which follow it from one
publisher to the next. The `Sample` pins its slot until it is dropped, and the
publisher lends no pinned slot (`SubscriberInfo::pinned` in the publisher's
stats shows which). By default the subscriber reads the slot with atomic
loads:

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_frames.rs:sample}}
```

The atomic copy is the default because the pool is memory the publisher's
process can write. A publisher that breaks the protocol and writes a pinned
slot can then make the subscriber read wrong bytes, and nothing worse: each
byte is loaded once, into the subscriber's own buffer, so a check made on the
copy still holds when the copy is used. A `&[u8]` over the slot would not have
that property — the bytes could change between a check and the use, which is
undefined behavior in Rust.

A subscriber that can vouch for the publisher's process — this system's own
service, say, and not a process less privileged than the subscriber — calls
`unsafe` `trust_publisher`. Its samples then deref to `[u8]` in place, and its
copies are plain memory copies. A `NamedSubscriber`'s `trust_publisher`
vouches for every publisher the name may be given to, the later ones included:

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_frames.rs:trusted}}
```

```text
cargo run -p rsbinder-topic --example topic_frames -- publish
cargo run -p rsbinder-topic --example topic_frames -- subscribe-trusted
```

`PoolPolicy::depth` (2 by default) is how many of the newest values stay
readable for a subscriber behind: each publish takes back the older ones,
which such a subscriber then gets as `Lagged`. A held `Sample` keeps its value
however old it grows. With backpressure nothing is taken back, and a loan
waits for a slot instead.

## Constraints

- **Loss is the default.** A subscriber that falls behind loses records;
  backpressure trades that for a publisher that waits and takes slow
  subscribers out.
- **One publisher.** Any thread of the publisher's process may publish; no
  other process writes the topic.
- **Kernel binder, or RPC on a Unix socket.** `Subscriber::request` refuses an
  RPC session that cannot pass fds or has no incoming connection; see
  [Topics without a binder device](#topics-without-a-binder-device). A subscriber
  of a publisher in its own process makes no binder call and needs no binder
  device.
- **The binder thread pool, over kernel binder.** A subscriber's process runs it
  (`ProcessState::start_thread_pool`) to learn that the publisher died —
  without it a wait for a record never ends — and that it was taken out. A
  `NamedSubscriber` needs it for the service manager's or the registry's
  notice of the name. The publisher's process needs it to answer subscription
  calls. Over a socket a session's own threads do this, and neither is needed.
- **Linux or Android.** The kernel must have `F_SEAL_FUTURE_WRITE` (mainline
  5.1; the Android common kernels from 4.14), and the target must be able to
  load a 64-bit word from a read-only mapping
  (`rsbinder::fmq::shm::RoWord64::PRESENT`). Elsewhere the crate compiles and
  every constructor returns `Unsupported`.
- **No binders or file descriptors in a value.** Neither means anything in the
  subscriber's process; the encoding refuses them (`BadType`,
  `FdsNotAllowed`).
- **Bounded mappings.** The publisher sizes the ring and the pool, and a
  subscriber maps them whole, so `SubscriberPolicy::max_ring_bytes` (16 MiB)
  and `max_pool_bytes` (256 MiB) refuse larger ones.
- **Do not copy the `.aidl`.** A topic is handed out as an `IBinder`; see
  [above](#handing-a-topic-out-from-an-interface).
