# Android Development

**rsbinder** provides comprehensive support for Android development alongside Linux. Since Android already has a complete Binder IPC environment, you can use **rsbinder**, **rsbinder-aidl**, and the existing Android service manager directly. There's no need to create binder devices or run a separate service manager like on Linux.

For building in the Android environment, you need to install the Android NDK and set up a Rust build environment that utilizes the NDK.

See: [Android Build Environment Setup](./android-build.md)

## Android Version Compatibility

**rsbinder** supports multiple Android versions with explicit feature flags for compatibility management. The Binder IPC interface has evolved across Android versions, and **rsbinder** handles these differences transparently.

### Supported Android Versions

- **Android 11 (API 30)**: `android_11` feature
- **Android 12 (API 31) / 12L (API 32)**: `android_12` feature
- **Android 13 (API 33)**: `android_13` feature
- **Android 14 (API 34)**: `android_14` feature
- **Android 16 (API 36)**: `android_16` feature

> **Note**: Android 12L (API 32) uses the same Binder protocol as Android 12, so both are covered by the `android_12` feature flag. Similarly, Android 15 (API 35) uses the same Binder protocol as Android 14, so it is covered by the `android_14` or `android_14_plus` feature flag. No separate `android_12l` or `android_15` feature is needed.

### Feature Flag Configuration

In your `Cargo.toml`, specify the Android versions you want to support:

```toml
[dependencies]
rsbinder = { version = "0.5", features = ["android_14_plus"] }
```

Available feature combinations:
- `android_11_plus`: Supports Android 11 through 16
- `android_12_plus`: Supports Android 12 through 16
- `android_13_plus`: Supports Android 13 through 16
- `android_14_plus`: Supports Android 14 through 16
- `android_16_plus`: Supports Android 16 only

### Protocol Compatibility

**rsbinder** maintains binary compatibility with Android's Binder protocol:

- **Transaction Format**: Uses identical `binder_transaction_data` structures
- **Object Types**: Supports all Android Binder object types (BINDER, HANDLE, FD)
- **Command Protocols**: Implements the same ioctl commands (BC_*/BR_* protocol)
- **Memory Management**: Compatible parcel serialization and shared memory handling
- **AIDL Compatibility**: Generates code compatible with Android's AIDL interfaces

### Version Detection (Optional)

**rsbinder** uses `rsproperties` internally to read Android system properties. You can use the same crate for version detection:

```rust
// Read Android SDK version (returns default value if not available)
let sdk_version: u32 = rsproperties::get_or("ro.build.version.sdk", 0);
println!("Android SDK version: {}", sdk_version);

// Read release version string
let version: String = rsproperties::get_or("ro.build.version.release", String::new());
println!("Running on Android {}", version);
```

Add `rsproperties` to your dependencies:
```toml
[dependencies]
rsproperties = "0.3"
```

### Using Android's Existing Binder Devices

On Android, the binder device files are already created and managed by the system. Use `ProcessState::init()` to connect to the appropriate device:

```rust
// Connect to the default system binder (/dev/binder)
ProcessState::init("/dev/binder", 0);
```

Android provides several binder devices for different purposes:

| Device | Service Manager | Description |
|--------|----------------|-------------|
| `/dev/binder` | `servicemanager` | Framework services (default) |
| `/dev/hwbinder` | `hwservicemanager` | HAL services (HIDL) |
| `/dev/vndbinder` | `vndservicemanager` | Vendor services |

> **Warning**: `hwbinder` uses a different protocol (`libhwbinder`) than standard binder (`libbinder`). **rsbinder** has not been tested with `hwbinder`, so compatibility is not guaranteed.

You do not need to run `rsb_hub` on Android — the system already provides service managers for each binder device.

### Android-Specific Considerations

- **Service Manager**: Uses Android's existing service manager automatically
- **Permissions**: Respects Android's security model and SELinux policies
- **Threading**: Integrates with Android's Binder thread pool management
- **Memory**: Uses Android's shared memory mechanisms (ashmem/memfd)
- **Stability**: Supports Android's interface stability annotations (@VintfStability)

### JNI Integration

**rsbinder** is designed for pure Rust-based programs. Integrating Rust Binder services with Java through JNI is **not recommended** and was not considered in the design. Since JNI only provides a C interface, multiple data conversions occur (Java → C → Rust), which is inefficient. Instead, develop independent Binder services in Rust and communicate with them from Java clients through the standard Binder IPC mechanism.
