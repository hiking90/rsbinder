#!/usr/bin/env bash
# Keep the lockfiles of the crates the root Cargo.toml `exclude`s in step with the workspace.
#
# bench/ipc-compare and fuzz are their own workspaces with their own Cargo.lock, and each lock
# records the version of every rsbinder crate they reach by path. A version bump leaves those
# entries at the old version until a cargo command in that crate rewrites them. `cargo metadata`
# changes a registry dependency only where a manifest no longer accepts the locked version (as when
# the workspace raises its tokio floor). It compiles nothing, so bench/ipc-compare's iceoryx2
# (Rust 1.89) and fuzz's nightly toolchain are not needed.
#
# Usage: excluded_lockfiles.sh           rewrite the out-of-date entries
#        excluded_lockfiles.sh --check   fail, naming the crate, when an entry is out of date
set -euo pipefail

cd "$(dirname "$0")/.."

case "${1:-}" in
    "") locked="" ;;
    --check) locked="--locked" ;;
    *) echo "usage: $0 [--check]" >&2; exit 2 ;;
esac

# The root Cargo.toml `exclude` list.
for crate in bench/ipc-compare fuzz; do
    if ! cargo metadata --format-version 1 $locked --manifest-path "$crate/Cargo.toml" > /dev/null; then
        [ -z "$locked" ] || echo "$crate/Cargo.lock is out of date: run scripts/excluded_lockfiles.sh" >&2
        exit 1
    fi
done
