#!/usr/bin/env bash
# Wait until test_service[_async] has finished registering all of its services.
#
# The integration suite restarts the service immediately before each destructive
# test. Checking only that the process is alive (`kill -0`) races with the
# service's asynchronous `addService` calls, so a fail-fast `getService` in the
# test can resolve nothing and panic ("did not get binder service"). The service
# prints `TEST_SERVICE_READY` to stderr once every service is registered; poll
# the (redirected) log for it, failing fast if the process dies or the marker
# never shows up.
#
# Usage: ci_wait_test_service_ready.sh <service-log> <pid-file>
set -u

log="${1:?usage: $0 <service-log> <pid-file>}"
pid_file="${2:?usage: $0 <service-log> <pid-file>}"
marker="TEST_SERVICE_READY"
deadline=$((SECONDS + 15))

while ! grep -q "$marker" "$log" 2>/dev/null; do
    if ! kill -0 "$(cat "$pid_file" 2>/dev/null)" 2>/dev/null; then
        echo "::error::test service process died before registering"
        cat "$log" 2>/dev/null || true
        exit 1
    fi
    if [ "$SECONDS" -ge "$deadline" ]; then
        echo "::error::test service did not register within 15s"
        cat "$log" 2>/dev/null || true
        exit 1
    fi
    sleep 0.1
done

echo "test service ready (all services registered)"
