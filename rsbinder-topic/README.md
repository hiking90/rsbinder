# rsbinder-topic

**Experimental.** One publisher and many subscribers over kernel binder, on
Linux and Android. A binder call sets each subscription up; after it the
records travel through shared memory, and no binder transaction is made for
them.

- The publisher allocates a ring of records in a sealed memfd, and every
  subscriber maps it read-only: one copy into the ring serves all of them.
- A publish does not wait for a subscriber. When the ring is full the oldest
  records are overwritten, and a subscriber that had not read them is told how
  many it lost. A topic with backpressure waits instead, up to a timeout, and
  then takes out the subscribers still in the way.
- A value is any AIDL parcelable, `#[derive(rsbinder::Parcelable)]` or
  generated from `.aidl`, or plain bytes. Large byte values go through a pool
  of slots the publisher writes in place, and a subscriber reads them where
  they lie.
- A topic is found through the service manager, by its binder or by a name
  that the subscriber watches and reconnects to.

A receiver that must see every value however slow it is uses
`rsbinder::stream` instead, which holds the sender back.

The [Topics chapter of the rsbinder book](https://hiking90.github.io/rsbinder/topics.html)
walks through a topic, its constraints, and when to trust a publisher. The
`examples/` directory holds the programs it quotes:

```text
cargo run -p rsbinder-topic --example topic_readings -- publish
cargo run -p rsbinder-topic --example topic_readings -- subscribe
```

Requirements: a kernel with `F_SEAL_FUTURE_WRITE` (mainline 5.1, the Android
common kernels from 4.14), and the binder thread pool running in both
processes. Elsewhere the crate compiles and every constructor returns
`Unsupported`.

The API and the shared-memory layout may change in any release; see the
[stability tiers](https://hiking90.github.io/rsbinder/stability-tiers.html).

Part of [rsbinder](https://github.com/hiking90/rsbinder). Apache-2.0.
