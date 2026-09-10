# Service Development

Once your interface compiles, the rest is the service that implements it and the client that calls it. This part of the guide collects the runtime patterns you will use day-to-day.

If you have not yet built a working service, start with [Hello, World!](./hello-world.md) and come back.

## Chapters

- **[Service Patterns](./service-patterns.md)** — The three pieces every rsbinder service needs (state struct, `impl Interface`, `impl IYourService`) and how to combine them for single- and multi-interface processes.
- **[Async Service](./async-service.md)** — Implementing services with `async fn` on top of the Tokio runtime, and how the async traits differ from their sync counterparts.
- **[Callbacks and Interfaces](./callbacks-and-interfaces.md)** — Passing `IBinder` objects in either direction, managing callback collections, and watching for remote process death.
- **[ParcelFileDescriptor](./parcel-file-descriptor.md)** — Sending pipes, files, and sockets across the Binder boundary as owned file descriptors.
- **[Shared Memory](./shared-memory.md)** — Sharing a mapped region instead of copying it, for payloads a transaction should not carry.
- **[Error Handling](./error-handling.md)** — The `StatusCode` / `Status` split, exception codes, and how AIDL methods surface both transport-level and application-level failures.
- **[Service Manager (HUB)](./service-manager.md)** — Registering, looking up, and waiting for services; differences between Linux (`rsb_hub`) and Android's native `servicemanager`; and how it relates to the [RPC transport](./rpc-transport.md).

Each builds on the one before it, but to ship a working service you only need **Service Patterns** and **Service Manager (HUB)**; the rest fill in capabilities as you need them.
