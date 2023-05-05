#!/bin/bash

export ANDROID_HOME=$HOME/Library/Android/sdk
if ! echo "$PATH" | grep -q -E "(^|:)$ANDROID_HOME/tools(:|$)"; then
    export PATH=$PATH:$ANDROID_HOME/tools:$ANDROID_HOME/tools/bin:$ANDROID_HOME/platform-tools
fi

function build() {
    cargo ndk -t x86_64 build --examples
}

function install() {
    adb push --sync target/x86_64-linux-android/debug/examples /data/rust/
}

function prepare() {
    adb root
}

prepare