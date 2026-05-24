# Service Development

Once your AIDL interface compiles, the next step is building the service that implements it and the client that calls it. This part of the guide collects the runtime patterns you will use day-to-day: how to structure a service, how to go async, how to model bidirectional communication, how to ship file descriptors, how to surface errors, and how to register and discover services through the HUB.

If you have not yet built a working service, start with [Hello, World!](./hello-world.md) and then return here for the deeper material.

## Chapters

- **[Service Patterns](./service-patterns.md)** — The three pieces every rsbinder service needs (state struct, `impl Interface`, `impl IYourService`) and how to combine them for single- and multi-interface processes.
- **[Async Service](./async-service.md)** — Implementing services with `async fn` on top of the Tokio runtime, and how the async traits differ from their sync counterparts.
- **[Callbacks and Interfaces](./callbacks-and-interfaces.md)** — Passing `IBinder` objects in either direction, managing callback collections, and watching for remote process death.
- **[ParcelFileDescriptor](./parcel-file-descriptor.md)** — Sending pipes, files, and sockets across the Binder boundary as owned file descriptors.
- **[Error Handling](./error-handling.md)** — The `StatusCode` / `Status` split, exception codes, and how AIDL methods surface both transport-level and application-level failures.
- **[Service Manager (HUB)](./service-manager.md)** — Registering, looking up, and waiting for services; differences between Linux (`rsb_hub`) and Android's native `servicemanager`; and how it relates to the [RPC transport](./rpc-transport.md).

## Suggested reading order

The chapters are designed to be read in roughly the order above — each builds on the previous one. If you only need to ship a working service quickly, **Service Patterns** plus **Service Manager (HUB)** is enough to get started; the other chapters fill in capabilities as you need them.
