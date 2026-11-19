# Migrating from 0.13 to 0.14

Every change that can break a program built against rsbinder 0.13.0 or
`rsbinder-topic` 0.1.0, the version released with it: a compile error, a
different value on the wire, or behavior a running program can observe.
`rsbinder-topic` goes to 0.2.0 with this release, as its API breaks. The entries
that change the bytes between peers say so, under *Wire*.

The additions and fixes themselves are listed in
[`CHANGELOG.md`](https://github.com/hiking90/rsbinder/blob/master/CHANGELOG.md).

## rsbinder

- `try_get_interface_async`, `check_interface_async`,
  `wait_for_interface_async` and `get_interface_async` are `fn`s returning
  `impl Future + Send + 'static`, and the future no longer borrows `name`.
  Called while handling a transaction, they and `connect_async` do their binder
  work before they return, so a future that is never polled has still made the
  lookup.
- `connect_async` on a kernel endpoint initializes `ProcessState` and starts
  the binder thread pool when it is called, wherever it is called. Call
  `ProcessState::init` with your settings before it: afterwards, other settings
  return `BadValue`. The wait for the name still starts at the first poll.
- `TokioRuntime::block_on` runs the call through `block_in_place`, instead of
  panicking, inside a current-thread `Handle::block_on` and on a multi-threaded
  runtime's worker that entered a current-thread handle. From a current-thread
  runtime's own `Runtime::block_on` body or within a `LocalSet` it still panics,
  with `block_in_place`'s message.
- Stream `*_async` waits (a futex wait on the ring, and on RPC a wait that is
  bounded or may ping) run on threads of rsbinder's own. `max_blocking_threads`
  does not bound them, and they no longer panic outside a Tokio runtime. An RPC
  stream's binder calls stay on the blocking pool, and panic outside one.
- `Reconnecting::connected`, called while handling a transaction, returns a
  connection that is up. It returns `WouldBlock` only when it would have to
  wait.
- `Reconnecting::current`, `connected`, `wait_connected` and `with`, finding
  that an unwatched RPC peer has closed, go on before the session's death
  recipients have run. Those run on the session's reaper thread.
- `end` after an awaited ring `send_async` whose record failed other than by
  timing out sends the caller's end. In 0.13.0 it sent `EX_ILLEGAL_STATE` naming
  that item.

The sections below are about `rsbinder-topic`.

## Making a publisher

`PublisherPolicy`, `PoolPolicy`, `Backpressure`, `Authorize`, `Publisher::new`,
`advertise` and `advertise_with` are gone, with no alias. A builder makes the
publisher, and a name is given after it:

| 0.1.0 | Now |
|---|---|
| `Publisher::new(&PublisherPolicy::default())` | `Publisher::<T>::events().build()` |
| `PublisherPolicy::ring_bytes` | `ring_bytes`, or `max_value_bytes` and `keep` |
| `PublisherPolicy::for_payload(max, depth)` | `max_value_bytes(max)` and `keep(depth)` |
| `max_subscribers`, `max_per_uid`, `authorize` fields | builder calls of the same names |
| `PoolPolicy` | `lend`, `max_loans`, `lend_keep`, `lend_huge_pages` |
| `Backpressure` | `lossless(timeout)`, `lossless_window(timeout, window)` |
| `Publisher::advertise(name, &policy)` | `build`, then `advertise_on(name, &ServiceManager)` |
| `Publisher::advertise_with(name, &policy, &resolver)` | `build`, then `advertise_on(name, &resolver)` |

A pool setting without `lend` is refused with `EX_ILLEGAL_ARGUMENT`.

**The default ring is 4 KiB, not 64 KiB.** A topic made with none of
`max_value_bytes`, `keep` and `ring_bytes` carries values of up to 2040 bytes,
where `PublisherPolicy::default()` carried 32760: a larger value now fails with
`PublishError::TooLarge`. With a sixteenth of the bytes, a subscriber that falls
behind loses records sooner, and a `lossless` publish waits sooner. Give
`max_value_bytes` the largest value the topic publishes, and `keep` the records
a subscriber may fall behind by. The ring is
`(keep + 1) × (max_value_bytes rounded up to 8, plus 8)` bytes and at least
4 KiB, with a `keep` of 0 counted as 1, and `max_value_bytes` taken as 8 when it
is not set and as 16 at least with `lend`.

## Making a subscriber

`SubscriberPolicy`, `Subscriber::request`, `NamedSubscriber::subscribe`,
`NamedSubscriber::subscribe_with`, `uds::subscribe` and `uds::subscribe_with`
are gone, with no alias. `Subscriber::<T>::builder()` takes `start_at`, `wake`,
`max_ring_bytes` and `max_pool_bytes`, and subscribes in its last call:

| 0.1.0 | Now |
|---|---|
| `Subscriber::request` | `subscribe(&binder)` |
| `NamedSubscriber::subscribe` | `subscribe_named(name)` |
| `NamedSubscriber::subscribe_with` | `subscribe_named_on(name, &resolver)` |
| `uds::subscribe` | `connect(endpoint)` |
| `uds::subscribe_with` | `connect_with(endpoint, &options)` |

`UdsResolver::subscribe` takes a `SubscriberBuilder` in place of the policy.

`Start::Latest` and `Start::Last` are now `Start::Next` and `Start::Newest`;
`Start::Oldest` keeps its name. The default start is the new `Start::Auto`,
which on an event topic is `Start::Next`, the 0.1.0 default.

On a 64-bit target a subscriber maps a ring of up to 256 MiB and a pool of up
to 1 GiB by default, up from 16 MiB and 256 MiB, so a publisher can make it
give up more address space, and make it read more pages into its resident
memory (about 1.25 GiB, from 272 MiB). The 32-bit defaults stay 16 MiB and
256 MiB.
`max_ring_bytes` and `max_pool_bytes` set the bound.

## Receiving

`try_latest`, `recv_latest` and the sample calls beside them, of a `Subscriber`
or a `NamedSubscriber`, on a topic that has ended, return the newest value the
subscriber has not returned and report the end on the next call. 0.1.0 reported
the end at once, and that value was not returned.

A subscriber with `Start::Newest` whose topic is closed as it subscribes returns
the topic's last value and then the end. 0.1.0 reported the end at once.

`recv_async` and `recv_sample_async` of a `Subscriber` or a `NamedSubscriber`,
with `Wake::Futex`, no longer need a Tokio runtime; 0.1.0 panicked outside one.
Their waits run on threads of rsbinder's own, which `max_blocking_threads` does
not bound. `RecvError::Runtime` with `ErrorKind::Other` now means no thread
could be started for the wait, where it meant the blocking pool did not run it.

A `NamedSubscriber` with `Start::Newest`, connected again to a publisher it read
before, starts at that publisher's newest record and steps over it when it is
the record returned last. 0.1.0 started at the next record, so a value published
while the name was elsewhere was never returned, and the subscriber got nothing
until the next publish. The same holds after a session over a socket ends.

A `NamedSubscriber` remembers the 170 publishers it connected to last. A
publisher connected to before those is read as one it never connected to, so a
record it returned can be returned again. 0.1.0 remembered every publisher until
a connection to it saw its end.

A dropped `Subscriber` tells its publisher at once (`ITopic.unsubscribed`), so
the publisher's count goes down without a publish; 0.1.0 waited for the next
publish or subscription. A reply the subscriber refuses tells the publisher
too.

## Implementing `ITopic` yourself

`ITopic` has two more methods, `unsubscribed` and `status`, and a type of the
application's own that implements it does not compile until it has them. Each
hands over to the `Acceptor` method of the same name, as `subscribe` hands over
to `accept`, and the type's `rsbinder::Interface::dump` to `Acceptor::dump`:

```rust,ignore
impl rsbinder::Interface for Guarded {
    fn dump(&self, writer: &mut dyn std::io::Write, args: &[String]) -> rsbinder::Result<()> {
        self.acceptor.dump(writer, args)
    }
}

impl ITopic for Guarded {
    // `r#subscribe` as before.
    fn r#unsubscribed(&self) -> BinderResult<()> {
        self.acceptor.unsubscribed();
        Ok(())
    }
    fn r#status(&self) -> BinderResult<TopicStatus> {
        self.acceptor.status()
    }
}
```

A type that implements `ITopic` only to check the caller needs none of this:
`PublisherBuilder::authorize` runs that check in the publisher's own binder.

`SubscriberInfo` has the fields `id` and `unwaited`; it is `#[non_exhaustive]`,
so only a pattern that names every field breaks. `Payload::type_name` reserves
the empty name, which `AnyType` sends, and a publisher of a type with it is
refused with `EX_ILLEGAL_ARGUMENT`.

## Wire

- **wire**: `TopicEndpoint` has a new last field, `topicKind`: `0` for an event
  topic and `1` for a state topic. The layout version is unchanged, so peers of
  either release subscribe to each other. A publisher built on `rsbinder-topic`
  0.1.0 sends no such field, and its topic is read as an event topic. A
  subscriber built on 0.1.0 ignores the field and reads a state topic as an
  event topic: with its default start, `Start::Latest`, it starts at the next
  record, so it does not get the current value as it subscribes, and it can
  receive `RecvError::Lagged`. A struct literal of `TopicEndpoint` names the
  field or ends in `..Default::default()`.
- A subscription whose `ITopic` reply has a `topicKind` other than `0` or `1`
  is refused with `BadValue`.
- **wire**: `SubscribeRequest` has a new last field, `flags`, and
  `TopicEndpoint` the last fields `id`, `flags` and `typeName`, in that order.
  `ITopic` has two more methods, `unsubscribed` (one way, code 1) and `status`
  (code 2). The layout version is unchanged, so peers of either release still
  subscribe to each other. Against a publisher built on 0.1.0: `unsubscribed`
  and `status` are `UNKNOWN_TRANSACTION`, the first never seen by its one-way
  caller; `Subscriber::id` is `None` and `topic_type` the subscriber's own type
  name; an `AnyType` subscriber is refused with `BadType`; and an `unwaited`
  subscriber is refused with `InvalidOperation` by its own side, which then
  tells the publisher to take the subscription out. A subscriber built on 0.1.0
  sends none of the fields, and its drop is seen at the publisher's next
  publish. A struct literal of either parcelable names the fields or ends in
  `..Default::default()`.
