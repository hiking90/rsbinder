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

    adb root
    if adb shell ls $remote_directory 1>/dev/null 2>&1; then
        echo "Directory already exists: $remote_directory"
    else
        echo "Directory does not exist, creating: $remote_directory"
        adb shell mkdir -p $remote_directory
    fi
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
