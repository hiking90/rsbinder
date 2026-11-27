# Replacing D-Bus

rsbinder does what a program uses D-Bus for (calling a named service,
hearing its signals, reading its properties and their changes) on a machine
with kernel binder, with three parts: binder calls for methods, `rsb_hub` for
names, and [topics](./topics.md) for signals. Nothing here speaks the D-Bus
protocol: a stock D-Bus client cannot reach an rsbinder service, and there is
no bridge.

The `rsbinder::bus` module and the `rsbinder-topic` crate are
[Experimental](./stability-tiers.md).

| D-Bus | rsbinder |
|---|---|
| Method call, reply, error name | A binder call on an interface defined in [AIDL](./aidl-guide.md) or with the [interface macro](./interface-macros.md); [`Status`](./error-handling.md) for the error |
| File descriptor in a message | [`ParcelFileDescriptor`](./parcel-file-descriptor.md) |
| Sender credentials | [`calling_caller()`](./security.md#caller-identity-inside-a-handler): uid, pid and SELinux context, filled in by the kernel for each call |
| Well-known name, `GetNameOwner` | `bus::system().add` and `check`; [the service manager](./service-manager.md) |
| `NameOwnerChanged`, waiting for a name | `Bus::watch`, `Bus::wait_for`; a service's death through [`link_to_death`](./callbacks-and-interfaces.md) |
| Bus activation (service files) | A [declaration with `start`](./service-manager.md#starting-a-service-on-demand) |
| Bus policy | [`rsb_hub` rules](./service-manager.md#access-control) by name; a method decides for itself with the caller's uid |
| Signal | An [event topic](./topics.md) |
| `Properties.Get`, `PropertiesChanged` | A [state topic](./topics.md#getting-and-setting-a-value) beside a getter, or [a list and its changes](#a-list-and-its-changes) |
| Session bus | `bus::session()` |
| `busctl list`, `busctl monitor` (signals) | `rsb_service list`, [`rsb_service topic echo`](./topics.md#inspecting-a-topic) |

## The system bus and the session bus

`rsbinder::bus::system()` is the names every process of the machine shares,
and `rsbinder::bus::session()` the names of the calling uid. Both return a
`Bus`, which adds, checks, waits for, watches and lists:

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_service.rs:session}}
```

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_service.rs:serve}}
```

Either function initializes the process's `ProcessState` on the default
binder device when nothing has. `Bus::watch` and the waits hear a
registration through a call into the process, so they need the binder thread
pool.

A session name is a name of the system's service manager with `user/<uid>/`
in front: `example.topic.units` of uid 1000 is `user/1000/example.topic.units`,
which is what `rsb_service` and the hub's log show, and what `Bus::name`
returns. The service manager's policy keeps each uid's names to that uid with
one rule:

```toml
[[rule]]
name = "user/{uid}/*"
add  = "any"
find = "any"
```

See [Access control](./service-manager.md#access-control) for how the rule
matches. Three things differ from a D-Bus session bus:

- **Every user opens the same binder device.** The rule stops another user
  from finding a name; it does not stop a call on a binder that a process of
  the session handed over by other means. A service that must refuse other
  users checks the caller's uid in its methods.
- **A service the hub starts runs with the hub's privileges.** A declaration
  of a `user/<uid>/` name is in the system hub's configuration, and its
  `start` is run by that hub. There is no per-user hub with a configuration
  the user writes.
- **The uid is the one the hub sees.** A process in a user namespace with
  another uid mapping asks for the name of the uid it sees, and a hub outside
  that namespace refuses it under this rule.

### One transport for the machine

A machine with a binder device uses kernel binder for both buses, and no
process chooses another transport for one of them: every process then asks
the same service manager, and a binder taken from one bus can be passed to a
service of the other.

- A machine with no binder device has no bus: `bus::system()` and
  `bus::session()` return `StatusCode::NameNotFound`, and no name of a service
  or of a topic is found there. What kernel binder does between the processes
  of a call, an RPC session does only between its two ends, so a bus on
  sockets could not behave as this one does.
- A device that exists and cannot be opened is that open's error. The process
  takes no part in the bus until the device's mode or group lets it in
  (`rsb_device --mode`, `--group`); it is not moved to another transport,
  which would leave it with names the rest of the machine does not see.
- A container that does not mount the host's binderfs is a machine of its
  own in this sense.

## Signals

A signal is a record of an event topic. The service makes a `Publisher`,
hands its binder out [from a method of its interface](./topics.md#handing-a-topic-out-from-an-interface)
or [under a name](./topics.md#finding-a-topic-by-name), and each client
subscribes. What differs from a D-Bus signal:

- **A subscriber that falls behind loses records and is told how many**
  (`RecvError::Lagged`). The publisher does not wait and does not queue for
  each subscriber: the records are in one ring that every subscriber maps.
  `PublisherBuilder::lossless` makes the publisher wait instead, up to a
  timeout, after which it takes the late subscriber out; see
  [Losing records, and backpressure](./topics.md#losing-records-and-backpressure).
- **Replies and records arrive on two paths.** A reply comes through binder,
  a record through the ring. A service that publishes before it replies
  leaves the caller's next receive with that record or a later one, which is
  the order a D-Bus connection gives.
- **Every subscriber of a topic reads the same records.** There is no match
  rule and no delivery to one receiver: a subscriber filters what it reads,
  and what only one client may see goes through a
  [callback](./callbacks-and-interfaces.md) to that client.
- **A topic has one publisher.**
- **A subscription costs a mapping**, the publisher's of the subscriber's
  control page and the subscriber's of the ring. A service has one topic with
  a field that says which object a record is about, not a topic for each
  object.

## Properties

A value that fits in one record is a state topic beside a getter and a
setter: a subscriber reads the current value as it subscribes and every later
one, with nothing to join. [Getting and setting a value](./topics.md#getting-and-setting-a-value)
has the example.

### A list and its changes

State that grows with the number of objects, such as the units of a service
manager, is not published whole on each change. A method returns the list, an
event topic carries each change, and a counter the service keeps joins the
two:

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_service.rs:values}}
```

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_service.rs:interface}}
```

The service changes its state, numbers the change and publishes it under one
lock, and reads the list and its revision under the same lock:

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_service.rs:service}}
```

A client subscribes, then asks for the list, then drops every change the
list already holds:

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_service.rs:watch}}
```

- **Subscribe first.** A change made between a list and a later subscription
  is in neither.
- **The lock is the service's.** The topic does not know the list; what makes
  the revision of a list the revision of its last change is that both are
  read and written under the lock the publish is made under.
- **After `Lagged`, list again.** The lost changes are in the new list. The
  same goes for a `NamedSubscriber`'s `RecvError::Disconnected`: the next
  publisher counts from its own start.
- **A record that carries the whole object needs no counter.** Applying, in
  order, every change received since the subscription leaves each object as
  its last change left it, whichever side of the list that change fell on;
  until the changes made before the list have been applied, an object can
  show a state older than the list's.

`cargo run -p rsbinder-topic --example topic_service -- serve`, then
`watch`, `start NAME` and `stop NAME` in other terminals, with `rsb_hub`
running.

## Names, activation and policy

- **Activation.** A `[[service]]` declaration with a `start` entry starts the
  service on a lookup that misses it, through `systemd` or an `exec`; see
  [Starting a service on demand](./service-manager.md#starting-a-service-on-demand).
  `Bus::wait_for` is such a lookup, `Bus::check` is not.
- **Policy.** `rsb_hub` decides who may add and find a name, and who may
  list. It does not see calls: a service that restricts a method checks the
  caller in the method ([Security & Authorization](./security.md)).
- **Name ownership.** A name is replaced by a later `add` of the same uid and
  refused to another uid while its service lives. There is no queue of
  waiting owners.
- **A restart of the hub.** The names end with `rsb_hub`, as a bus's do with
  its daemon, and the init system restarts the services that depend on it;
  see [When the hub restarts](./service-manager.md#when-the-hub-restarts).
  Unlike a D-Bus connection, a call on a binder a client already holds goes
  on while the hub is down.

## Looking at a running system

```bash
$ rsb_service list                         # names with a service
$ rsb_service wait user/1000/example.topic.units --timeout 30
$ rsb_service dump user/1000/example.topic.units
$ rsb_service topic echo NAME              # the records of a topic, as they are published
$ rsb_service topic stat NAME              # its subscribers, their lag and losses
```

`rsb_service topic` needs the `topic` feature of `rsbinder-tools`; it finds a
topic in the hub's topic registry, or with `--service` under a service name.
It prints a record's type name and bytes: it has no schema to decode them
with.

## What there is not

| D-Bus | Why not |
|---|---|
| The D-Bus wire protocol, names and introspection | Not a goal: an interface is its AIDL, known at build time |
| `busctl call` on any method | A call needs the interface's definition to build its parcel |
| `busctl monitor` of method calls | A binder call goes from the caller to the service, with no broker between them to watch. The kernel's binder tracepoints show transactions |
| Match rules in the bus | Subscribers filter |
| Name request queues, `ALLOW_REPLACEMENT` | See name ownership above |
| A bus on a machine with no binder device | No; see [One transport for the machine](#one-transport-for-the-machine) |
