#!/bin/bash

# Check if TOP_DIR is already set
if [ -z "$TOP_DIR" ]; then
    # Set TOP_DIR to the current working directory if it's not already set
    TOP_DIR=$(pwd)
    export TOP_DIR
else
    echo "TOP_DIR is already set to $TOP_DIR."
fi

# Check the operating system
os_name=$(uname)

if [ "$os_name" = "Darwin" ]; then
    export ANDROID_HOME=$HOME/Library/Android/sdk
elif [ "$os_name" = "Linux" ]; then
fi

if ! echo "$PATH" | grep -q -E "(^|:)$ANDROID_HOME/tools(:|$)"; then
    export PATH=$PATH:$ANDROID_HOME/tools:$ANDROID_HOME/tools/bin:$ANDROID_HOME/platform-tools
fi

function build() {
    cargo ndk -t x86_64 build
}

function install() {
    adb push --sync target/x86_64-linux-android/debug/ /data/rsbinder/
}

function prepare() {
    adb root
}

function aidl_gen_rust() {
    aidl --lang=rust -I $TOP_DIR/aidl $1 -o gen
}

prepare
