#!/usr/bin/env bash
# Wait until rsb_hub holds handle 0.
#
# A fixed sleep before `kill -0` proves only that the process is alive: under
# load the hub can still be initializing, and a test service started then finds
# no service manager. The hub logs `serving on` at info level right after it
# becomes the context manager, so run it with RUST_LOG=info; poll the
# (redirected) log for that line, failing fast if the process dies or the line
# never shows up.
#
# Usage: ci_wait_hub_ready.sh <hub-log> <pid>
set -u

log="${1:?usage: $0 <hub-log> <pid>}"
pid="${2:?usage: $0 <hub-log> <pid>}"
marker="rsb_hub: serving on"
deadline=$((SECONDS + 30))

while ! grep -q "$marker" "$log" 2>/dev/null; do
    if ! kill -0 "$pid" 2>/dev/null; then
        echo "::error::rsb_hub died before serving"
        cat "$log" 2>/dev/null || true
        exit 1
    fi
    if [ "$SECONDS" -ge "$deadline" ]; then
        echo "::error::rsb_hub did not serve within 30s"
        cat "$log" 2>/dev/null || true
        exit 1
    fi
    sleep 0.1
done
