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
    cargo ndk --no-strip -t $ndk_target build && cargo ndk --no-strip -t $ndk_target -- test --no-run
}

function ndk_sync() {
    read_remote_android

    if [[ "$OSTYPE" == "darwin"* ]]; then
        # macOS
        find_command="find \"$source_directory\" -maxdepth 1 -type f -perm +111"
    else
        # Linux
        find_command="find \"$source_directory\" -maxdepth 1 -type f -executable"
    fi

    eval $find_command | while read file; do
        adb push "$file" "$remote_directory/"
    done
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
        echo "x86_64"
        echo "/data/rsbinder"
        exit 1
    fi

    {
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