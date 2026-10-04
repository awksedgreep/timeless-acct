#!/bin/sh
# Install timeless-acct as a service and start it: the binary, its user,
# and its unit. The user who ran this through sudo joins the group that
# may watch it.
#
#   cargo build --release
#   sudo dist/install.sh               install, or upgrade, and start
#   sudo dist/install.sh --uninstall   stop and remove; the store is kept
set -eu

here=$(cd "$(dirname "$0")" && pwd)
binary=$here/../target/release/timeless-acct

if [ "$(id -u)" != 0 ]; then
    echo "as root, please: sudo $0 $*" >&2
    exit 1
fi

if [ "${1:-}" = --uninstall ]; then
    systemctl disable --now timeless-acct 2>/dev/null || true
    rm -f /etc/systemd/system/timeless-acct.service /usr/local/bin/timeless-acct
    systemctl daemon-reload
    echo "timeless-acct is stopped and removed."
    echo "Its store is kept in /var/lib/timeless-acct, and its user and group;"
    echo "remove them with: rm -r /var/lib/timeless-acct; userdel timeless-acct"
    exit 0
fi

if [ ! -x "$binary" ]; then
    echo "no binary at $binary: run cargo build --release first" >&2
    exit 1
fi

install -Dm 0755 "$binary" /usr/local/bin/timeless-acct
install -Dm 0644 "$here/timeless-acct.sysusers" /etc/sysusers.d/timeless-acct.conf
systemd-sysusers /etc/sysusers.d/timeless-acct.conf
install -Dm 0644 "$here/timeless-acct.service" /etc/systemd/system/timeless-acct.service
systemctl daemon-reload
systemctl enable timeless-acct
# Started, or restarted on the new binary if it was running.
systemctl restart timeless-acct
echo "timeless-acct is recording into /var/lib/timeless-acct."

if [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != root ]; then
    if id -nG "$SUDO_USER" | tr ' ' '\n' | grep -qx timeless-acct; then
        echo "Watch it with: timeless-acct watch"
    else
        usermod -aG timeless-acct "$SUDO_USER"
        echo "$SUDO_USER may watch it after logging in again: timeless-acct watch"
    fi
fi
