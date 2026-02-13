# Getting Started

Welcome to **rsbinder**! This guide will help you get started with Binder IPC development using Rust.

## Learning Path

If you are new to Binder IPC, we recommend following this learning path:

1. **[Architecture](./architecture.md)** - Start here to understand Binder IPC fundamentals
   - Learn about the core concepts and components
   - Understand the relationship between services and clients
   - See how AIDL generates Rust code

2. **[Installation](./installation.md)** - Set up your development environment
   - Install required dependencies
   - Set up binder devices and service manager
   - Configure your Rust project

3. **[Hello World](./hello-world.md)** - Build your first Binder service
   - Create a simple echo service
   - Learn AIDL basics
   - Understand service registration and client communication

4. **Platform-specific Setup** - Choose your target platform:
   - **[Linux Setup](./enable-binder-for-linux.md)** - For Linux development
   - **[Android Development](./android.md)** - For Android integration

## Quick Start Checklist

Before diving into development, ensure you have:

- [ ] Rust 1.77+ installed
- [ ] Linux kernel with binder support enabled
- [ ] Created binder device using `rsb_device`
- [ ] Service manager (`rsb_hub`) running
- [ ] Basic understanding of AIDL syntax

## Key Concepts to Understand

- **Services**: Server-side implementations that provide functionality
- **Clients**: Applications that consume services through proxies
- **AIDL**: Interface definition language for describing service contracts
- **Service Manager**: Central registry for service discovery
- **Parcels**: Serialization format for data exchange
- **Binder Objects**: References that enable cross-process communication

## Common Development Workflow

1. Define your service interface in an `.aidl` file
2. Use `rsbinder-aidl` to generate Rust code
3. Implement your service logic
4. Register the service with the service manager
5. Create clients that discover and use your service

Ready to start? Head to the [Architecture](./architecture.md) section to learn the fundamentals!
