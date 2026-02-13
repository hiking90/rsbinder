# Android Development

**rsbinder** provides comprehensive support for Android development alongside Linux. Since Android already has a complete Binder IPC environment, you can use **rsbinder**, **rsbinder-aidl**, and the existing Android service manager directly. There's no need to create binder devices or run a separate service manager like on Linux.

For building in the Android environment, you need to install the Android NDK and set up a Rust build environment that utilizes the NDK.

See: [Android Build Environment Setup](./android-build.md)

## Android Version Compatibility

**rsbinder** supports multiple Android versions with explicit feature flags for compatibility management. The Binder IPC interface has evolved across Android versions, and **rsbinder** handles these differences transparently.

### Supported Android Versions

- **Android 11 (API 30)**: `android_11` feature
- **Android 12 (API 31)**: `android_12` feature
- **Android 13 (API 33)**: `android_13` feature
- **Android 14 (API 34)**: `android_14` feature
- **Android 16 (API 36)**: `android_16` feature

### Feature Flag Configuration

In your `Cargo.toml`, specify the Android versions you want to support:

```toml
[dependencies]
rsbinder = { version = "0.4.0", features = ["android_14_plus"] }
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

For applications that need to support multiple Android versions dynamically:

```rust
use android_system_properties::AndroidSystemProperties;

let properties = AndroidSystemProperties::new();

if let Some(version) = properties.get("ro.build.version.release") {
    // Handle version-specific logic if needed
    println!("Running on Android {}", version);
}
```

### Android-Specific Considerations

- **Service Manager**: Uses Android's existing service manager automatically
- **Permissions**: Respects Android's security model and SELinux policies
- **Threading**: Integrates with Android's Binder thread pool management
- **Memory**: Uses Android's shared memory mechanisms (ashmem/memfd)
- **Stability**: Supports Android's interface stability annotations (@VintfStability)
