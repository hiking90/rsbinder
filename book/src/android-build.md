# Android Build Environment Setup

This guide will help you set up a complete Android development environment for building and testing **rsbinder** applications.

## Prerequisites

### Android SDK Installation

You need to install the Android SDK which includes essential tools like `adb` (Android Debug Bridge) and other platform tools required for Android development.

#### Method 1: Android Studio (Recommended)
Download and install Android Studio, which includes the Android SDK:
- [Android Studio Download](https://developer.android.com/studio)

#### Method 2: Command Line Tools Only
If you prefer a minimal installation:
1. Download Command Line Tools from [Android Developer Downloads](https://developer.android.com/studio#command-tools)
2. Extract and set up the SDK manager

### Required SDK Components

Install the following SDK components using the SDK Manager:

```bash
# Install platform-tools (includes adb)
$ sdkmanager "platform-tools"

# Install emulator (for testing)
$ sdkmanager "emulator"

# Install system images for testing (choose your target API levels)
$ sdkmanager "system-images;android-30;google_apis;x86_64"
$ sdkmanager "system-images;android-34;google_apis;x86_64"
```

### Android NDK Installation

The Android NDK (Native Development Kit) is required for building native Rust code for Android.

#### Method 1: Through Android Studio
- Open Android Studio
- Go to Tools → SDK Manager → SDK Tools
- Check "NDK (Side by side)" and install

#### Method 2: Command Line Installation
```bash
# Install specific NDK version
$ sdkmanager "ndk;26.1.10909125"  # Latest stable version
# or
$ sdkmanager "ndk;25.2.9519653"   # Alternative stable version
```

#### Method 3: Direct Download
Download from the [NDK Downloads page](https://developer.android.com/ndk/downloads) and extract manually.

### Environment Setup

Set up environment variables in your shell profile (`.bashrc`, `.zshrc`, etc.):

```bash
# Android SDK
export ANDROID_HOME=$HOME/Android/Sdk  # Linux
# export ANDROID_HOME=$HOME/Library/Android/sdk  # macOS

# Android NDK
export ANDROID_NDK_ROOT=$ANDROID_HOME/ndk/26.1.10909125
export NDK_HOME=$ANDROID_NDK_ROOT

# Add tools to PATH
export PATH=$PATH:$ANDROID_HOME/cmdline-tools/latest/bin
export PATH=$PATH:$ANDROID_HOME/platform-tools
export PATH=$PATH:$ANDROID_HOME/emulator
```

### Rust Toolchain Setup

#### Install cargo-ndk
```bash
$ cargo install cargo-ndk --version "^3.0"
```

#### Add Android targets
```bash
$ rustup target add aarch64-linux-android
$ rustup target add x86_64-linux-android
$ rustup target add armv7-linux-androideabi
$ rustup target add i686-linux-android
```

## Building rsbinder for Android

### Basic Build Commands

```bash
# Build for ARM64 (most common for modern Android devices)
$ cargo ndk -t aarch64-linux-android build --release

# Build for x86_64 (emulator)
$ cargo ndk -t x86_64-linux-android build --release

# Build all targets
$ cargo ndk -t aarch64-linux-android -t x86_64-linux-android build --release
```

### Using envsetup.sh Helper Scripts

The **rsbinder** project provides a comprehensive `envsetup.sh` script with helpful functions for Android development:

```bash
# Source the environment setup
$ source ./envsetup.sh
```

#### Available Functions

**`ndk_prepare`**: Sets up the Android device for testing
- Roots the device using `adb root`
- Creates the remote directory on the device (`/data/rsbinder` by default)
- Prepares the environment for file synchronization

```bash
$ ndk_prepare
```

**`ndk_build`**: Builds the project for Android
- Reads configuration from `REMOTE_ANDROID` file
- Builds both release binaries and test executables
- Uses `cargo ndk` with the specified target architecture

```bash
$ ndk_build
```

**`ndk_sync`**: Synchronizes built binaries to Android device
- Pushes all executable files to the device
- Uses `adb push` to transfer files to the remote directory
- Automatically detects executable files in the target directory

```bash
$ ndk_sync
```

### Configuration File: REMOTE_ANDROID

Create a `REMOTE_ANDROID` file in the project root to configure your Android target:

```bash
# Example REMOTE_ANDROID file
arm64-v8a           # cargo-ndk target
aarch64             # rust target architecture
/data/rsbinder      # remote directory on device
```

Common target configurations:
```bash
# For ARM64 devices (most modern Android phones)
arm64-v8a
aarch64
/data/rsbinder

# For x86_64 emulator
x86_64
x86_64
/data/rsbinder
```

## Testing on Android

### Device Setup
1. Enable Developer Options on your Android device
2. Enable USB Debugging
3. Connect device via USB and authorize debugging

### Running Tests
```bash
# Complete build and test workflow
$ source ./envsetup.sh
$ ndk_prepare      # Set up device
$ ndk_build        # Build binaries and tests
$ ndk_sync         # Push to device

# Test executables will be available in /data/rsbinder/
```

### Emulator Testing
```bash
# Create and start an emulator
$ avdmanager create avd -n test_device -k "system-images;android-34;google_apis;x86_64"
$ emulator -avd test_device

# Build for emulator target
$ cargo ndk -t x86_64-linux-android build --release
```

## Troubleshooting

### Common Issues

**"adb not found"**: Ensure platform-tools is installed and in PATH
**"cargo-ndk not found"**: Install with `cargo install cargo-ndk`
**"No targets specified"**: Add Android targets with `rustup target add`
**"Permission denied on device"**: Run `adb root` or check device permissions

### Verification Commands
```bash
# Check SDK installation
$ sdkmanager --list_installed

# Check connected devices
$ adb devices

# Check available targets
$ rustup target list | grep android

# Test cargo-ndk installation
$ cargo ndk --version
```

