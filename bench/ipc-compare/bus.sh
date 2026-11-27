#!/usr/bin/env bash
# A dbus-broker of the measurement's own, apart from the session bus.
#
#   bus.sh start    prints the address to export as DBUS_SESSION_BUS_ADDRESS
#   bus.sh stop
#   bus.sh info     version and limits, for conditions.txt
#
# dbus-broker-launch takes its listening socket from systemd, so the bus is a transient socket
# unit of the user manager (`systemd-run --user`): the launcher does not start on its own, and
# `systemd-socket-activate` hands the socket over in a way it resets connections on.
set -euo pipefail

unit=rsb-bench-dbus
dir=${XDG_RUNTIME_DIR:?bus.sh: XDG_RUNTIME_DIR is not set}/$unit

case "${1:-}" in
    start)
        for tool in dbus-broker-launch systemd-run; do
            command -v "$tool" >/dev/null || { echo "bus.sh: $tool not found" >&2; exit 1; }
        done
        systemctl --user stop "$unit.socket" "$unit.service" 2>/dev/null || true
        systemctl --user reset-failed "$unit.socket" "$unit.service" 2>/dev/null || true
        rm -rf "$dir"
        mkdir -p "$dir"
        cat > "$dir/bus.conf" <<EOF
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path=$dir/bus</listen>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow receive_sender="*"/>
    <allow own="*"/>
  </policy>
</busconfig>
EOF
        systemd-run --user --quiet --unit="$unit" \
            --socket-property=ListenStream="$dir/bus" --service-type=notify \
            dbus-broker-launch --scope user --config-file "$dir/bus.conf"
        echo "unix:path=$dir/bus"
        ;;
    stop)
        systemctl --user stop "$unit.socket" "$unit.service" 2>/dev/null || true
        rm -rf "$dir"
        ;;
    info)
        echo "dbus-broker: $(dbus-broker --version 2>/dev/null | head -1)"
        pid=$(pgrep -u "$(id -u)" -n -x dbus-broker || true)
        echo "dbus-broker command line: $([ -n "$pid" ] && tr '\0' ' ' < "/proc/$pid/cmdline")"
        ;;
    *)
        echo "usage: bus.sh start|stop|info" >&2
        exit 2
        ;;
esac
