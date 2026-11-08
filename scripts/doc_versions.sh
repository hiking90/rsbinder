#!/usr/bin/env bash
# Keep the dependency lines the documentation shows in step with the manifests.
#
# READMEs, book pages and crate docs tell a reader what to put in Cargo.toml:
# `rsbinder = "0.13"`, `rsbinder-aidl = { version = "0.13", features = [..] }`. No cargo command
# reads them, so a version bump leaves them behind. This script sets the version in each such line
# to the major.minor of the manifest it names: `[workspace.package] version` for `rsbinder`,
# `rsbinder-aidl`, `rsbinder-macros` and `rsbinder-fmq`, `rsbinder-topic/Cargo.toml` for
# `rsbinder-topic`. A line is recognized only in those two forms with a plain number; one with an
# operator (`"=0.13.0"`) or a path is left alone. CHANGELOG.md and book/src/migrating-*.md describe
# past releases and are not read.
#
# Usage: doc_versions.sh           rewrite the out-of-date lines
#        doc_versions.sh --check   fail, printing the diff, when a line is out of date
set -euo pipefail

cd "$(dirname "$0")/.."

case "${1:-}" in
    "") check="" ;;
    --check) check=1 ;;
    *) echo "usage: $0 [--check]" >&2; exit 2 ;;
esac

# The major.minor of the first `version = "X.Y.Z"` after the line matching $2 in $1.
major_minor() {
    sed -n -E "/$2/,\$ s/^version = \"([0-9]+\.[0-9]+)\.[0-9]+\"/\1/p" "$1" | head -n 1
}

workspace=$(major_minor Cargo.toml '^\[workspace\.package\]')
topic=$(major_minor rsbinder-topic/Cargo.toml '^\[package\]')
if [ -z "$workspace" ] || [ -z "$topic" ]; then
    echo "$0: no X.Y.Z version in Cargo.toml [workspace.package] or rsbinder-topic/Cargo.toml" >&2
    exit 1
fi

# One expression per version source; the leading class keeps `my-rsbinder = ..` out.
expr_workspace='s/(^|[^A-Za-z0-9_-])(rsbinder(-aidl|-macros|-fmq)? = (\{ *version = )?")[0-9]+(\.[0-9]+)*"/\1\2'"$workspace"'"/g'
expr_topic='s/(^|[^A-Za-z0-9_-])(rsbinder-topic = (\{ *version = )?")[0-9]+(\.[0-9]+)*"/\1\2'"$topic"'"/g'

stale=""
for f in $(git ls-files '*.md' '*/src/lib.rs' ':!CHANGELOG.md' ':!book/src/migrating-*.md'); do
    if sed -E -e "$expr_workspace" -e "$expr_topic" "$f" | cmp -s "$f" -; then
        continue
    fi
    if [ -n "$check" ]; then
        sed -E -e "$expr_workspace" -e "$expr_topic" "$f" | diff -u "$f" - || true
        stale=1
    else
        # `-i.bak` is the in-place form GNU and BSD sed both take.
        sed -i.bak -E -e "$expr_workspace" -e "$expr_topic" "$f"
        rm "$f.bak"
        echo "updated $f"
    fi
done

if [ -n "$stale" ]; then
    echo "documented dependency versions are out of date: run scripts/doc_versions.sh" >&2
    exit 1
fi
