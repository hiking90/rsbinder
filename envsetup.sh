#!/bin/bash

# Check if TOP_DIR is already set
if [ -z "$TOP_DIR" ]; then
    # Set TOP_DIR to the current working directory if it's not already set
    TOP_DIR=$(pwd)
    export TOP_DIR
else
    echo "TOP_DIR is already set to $TOP_DIR."
fi

if [[ "$OSTYPE" == "darwin"* ]]; then
    export ANDROID_HOME=$HOME/Library/Android/sdk
elif [ "$OSTYPE" = "linux"* ]; then
    export ANDROID_HOME=$HOME/Android/Sdk
fi

if ! echo "$PATH" | grep -q -E "(^|:)$ANDROID_HOME/tools(:|$)"; then
    export PATH=$PATH:$ANDROID_HOME/tools:$ANDROID_HOME/tools/bin:$ANDROID_HOME/platform-tools
fi

function ndk_build() {
    read_remote_android
    cargo ndk -t $cargo_ndk_target build && cargo ndk -t $cargo_ndk_target -- test --no-run
}

function ndk_sync() {
    read_remote_android

    echo "Syncing binaries from $source_directory to $remote_directory..."

    # Sync main executables from debug directory
    if [[ "$OSTYPE" == "darwin"* ]]; then
        # macOS
        find_command="find \"$source_directory\" -maxdepth 1 -type f -perm +111"
    else
        # Linux
        find_command="find \"$source_directory\" -maxdepth 1 -type f -executable"
    fi

    eval $find_command | while read file; do
        echo "  Pushing $(basename "$file")..."
        adb push "$file" "$remote_directory/"
    done

    # Sync test binaries from deps directory
    local deps_directory="$source_directory/deps"
    if [ -d "$deps_directory" ]; then
        echo "Syncing test binaries from deps directory..."

        if [[ "$OSTYPE" == "darwin"* ]]; then
            # macOS - find test executables (exclude .d files and libraries)
            find "$deps_directory" -type f -perm +111 ! -name "*.d" ! -name "*.so" ! -name "*.dylib" | while read file; do
                echo "  Pushing $(basename "$file")..."
                adb push "$file" "$remote_directory/"
            done
        else
            # Linux
            find "$deps_directory" -type f -executable ! -name "*.d" ! -name "*.so" | while read file; do
                echo "  Pushing $(basename "$file")..."
                adb push "$file" "$remote_directory/"
            done
        fi
    fi

    echo "Sync complete!"
}

function read_remote_android() {
    file="REMOTE_ANDROID"

    if [ ! -f "$file" ]; then
        echo "The file '$file' does not exist."
        echo "Please create the '$file' file with the following format:"
        echo
        echo "Please use the cargo ndk target information on the first line"
        echo "and the remote directory information on the second line."
        echo
        echo "Example:"
        echo "arm64-v8a"
        echo "aarch64"
        echo "/data/rsbinder"
        exit 1
    fi

    {
        read cargo_ndk_target
        read ndk_target
        read remote_directory
    } <"$file"

    source_directory="$TOP_DIR/target/$ndk_target-linux-android/debug"
}

function ndk_prepare() {
    read_remote_android

    # Not fatal on its own: a production/PlayStore image refuses root, and
    # the real symptom is then the mkdir below failing with a message that
    # names the directory. Letting a non-zero exit here abort the caller
    # (CI runs under `bash -e`) would hide that.
    adb root || echo "ndk_prepare: 'adb root' failed; $remote_directory may be unwritable"
    # `adb root` restarts adbd, so the device detaches and re-attaches.
    # Without this the next command races that reconnect.
    adb wait-for-device

    # A guest that has just set `sys.boot_completed` is still bringing up
    # zygote/system_server and can be short on memory, so an `adb shell`
    # there dies with `fork failed: Out of memory` (exit 126). Retry
    # instead of letting one transient failure abort the caller — CI runs
    # this step under `bash -e`, where a single non-zero exit is fatal.
    # `mkdir -p` is idempotent, so this doubles as the existence check.
    local tries=0
    until adb shell "mkdir -p $remote_directory" 2>/dev/null; do
        tries=$((tries + 1))
        if [ "$tries" -ge 30 ]; then
            echo "ndk_prepare: cannot create $remote_directory (gave up after $tries tries)" >&2
            return 1
        fi
        echo "ndk_prepare: guest not ready yet, retrying ($tries/30)..."
        sleep 2
    done
    echo "Directory ready: $remote_directory"
}

function aidl_gen_rust() {
    aidl --lang=rust -I $TOP_DIR/aidl $1 -o gen
}

function read_remote_linux() {
    file="REMOTE_LINUX"

    if [ ! -f "$file" ]; then
        echo "The file '$file' does not exist."
        echo "Please create the '$file' file with the following format:"
        echo
        echo "userid@remote-ip-address"
        echo "/path/to/remote/directory"
        echo
        echo "Example:"
        echo "alice@192.168.1.100"
        echo "/home/alice/work"
        exit 1
    fi

    {
        read remote_user_host
        read remote_directory
    } <"$file"
}

function remote_sync() {
    read_remote_linux
    command rsync -avz --exclude-from='.gitignore' --exclude '.git' $TOP_DIR/ "$remote_user_host:$remote_directory"
}

function remote_shell() {
    read_remote_linux
    command ssh "$remote_user_host" -t "cd $remote_directory; bash"
}

function remote_test() {
    read_remote_linux
    remote_sync
    command ssh "$remote_user_host" -t "bash -c \"source ~/.profile && cd $remote_directory && \
        source ./envsetup.sh && run_test \""
}

function run_test() {
    local MAX_TRIES=${1:-"100"}
    cargo run --bin rsb_hub & sleep 1
    cargo run --bin test_service & sleep 1
    for i in $(seq 1 $MAX_TRIES); do RUST_BACKTRACE=1 cargo test || break; done
    cargo test test_death_recipient -- --ignored
}

function remote_test_async() {
    read_remote_linux
    remote_sync
    command ssh "$remote_user_host" -t "bash -c \"source ~/.profile && cd $remote_directory && \
        source ./envsetup.sh && run_test_async \""
}

function run_test_async() {
    local MAX_TRIES=${1:-"100"}
    cargo run --bin rsb_hub & sleep 1
    cargo run --bin test_service_async & sleep 1
    for i in $(seq 1 $MAX_TRIES); do RUST_BACKTRACE=1 cargo test || break; done
    cargo test test_death_recipient -- --ignored
}

function remote_coverage() {
    read_remote_linux
    remote_sync
    command ssh "$remote_user_host" -t "bash -c \"source ~/.profile && cd $remote_directory && \
        source ./envsetup.sh && run_coverage \""
    # Restore rustup override
    command ssh "$remote_user_host" -t "bash -c \"source ~/.profile && cd $remote_directory && \
        source ./envsetup.sh && rustup override unset \""
    # Sync coverage report
    command rsync -avz "$remote_user_host:$remote_directory/coverage" $TOP_DIR
}

function run_coverage() {
    rustup override set nightly
    if [ $? -ne 0 ]; then
        echo "Failed to set nightly toolchain. Exiting shell."
        exit 1
    fi
    export RUSTFLAGS="-Zprofile -Ccodegen-units=1 -Clink-dead-code"
    export CARGO_INCREMENTAL=0
    export RUSTDOCFLAGS="-Cpanic=abort"
    export CARGO_TARGET_DIR="target/coverage"
    cargo clean && cargo build && cargo test --no-run
    (
        run_test 1
    )
    (
        run_test_async 1
    )

    rm -rf coverage && grcov . -s . --binary-path ./target/debug -t html -o coverage \
        --ignore "example-hello/*" \
        --ignore "target/*" \
        --ignore "tests/*" \
        --ignore "rsbinder-aidl/tests/*"

    rustup override unset
}

declare -a publish_dirs=("rsbinder-aidl" "rsbinder" "rsbinder-tools")

function publish() {
    local cargo_options=()
    if [[ "$1" == "--dry-run" ]]; then
        cargo_options=("$1" "--allow-dirty")
    fi

    for dir in "${publish_dirs[@]}"; do
        echo "Publishing $dir with options: $cargo_options"
        pushd "$dir" > /dev/null

        cargo publish "${cargo_options[@]}"
        local result=$?

        popd > /dev/null
        if [ $result -ne 0 ]; then
            echo "Error occurred in $dir, exiting..."
            return $result
        fi
    done
    return 0
}

function publish_dry_run() {
    publish --dry-run
}

function version_update() {
    local NEW_VERSION="$1"

    find . -name "Cargo.toml" -exec sed -i '' "s/^version = \".*\"/version = \"$NEW_VERSION\"/" {} \;
    find . -name "Cargo.toml" -exec sed -i '' "/version = \"[^\"]*\", path =/ s/version = \"[^\"]*\"/version = \"$NEW_VERSION\"/" {} \;
}
