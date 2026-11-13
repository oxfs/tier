#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
binary=${1:-"$root/bin/tier"}
module=${2:-"$root/lib/tier.so"}
[[ $EUID == 0 ]] || {
    echo "Run as root." >&2
    exit 1
}
# shellcheck source=/dev/null
source /etc/os-release
[[ $ID == debian && $VERSION_ID == 13 ]] || {
    echo "Debian 13 is required." >&2
    exit 1
}
[[ -f $binary && -f $module ]] || {
    echo "Build the daemon and Samba module first." >&2
    exit 1
}
if [[ -f "$root/deploy/samba-version" ]]; then
    [[ $(dpkg-query -W -f='${Version}' samba) == "$(cat "$root/deploy/samba-version")" ]] || {
        echo 'Samba package version differs from this module build; rebuild the module for the installed package.' >&2
        exit 1
    }
fi
modules=$(smbd -b | awk '$1 == "MODULESDIR:" { print $2 }')
[[ -d "$modules/vfs" ]]
if ! id tier >/dev/null 2>&1; then
    if ! getent group tier >/dev/null; then
        groupadd --system tier
    fi
    useradd --system --gid tier --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin tier
fi
install -m 0755 "$binary" /usr/local/bin/tier
install -m 0755 "$module" "$modules/vfs/tier.so"
install -m 0644 "$root/deploy/tier.service" /etc/systemd/system/tier.service
install -d -m 0750 -o root -g tier /etc/tier
if [[ -d /run/systemd/system ]]; then
    systemctl daemon-reload
fi
echo 'Installed. Configure /etc/tier/config.toml (readable by tier), make storage roots writable by tier, then run:'
echo '  runuser -u tier -- tier /etc/tier/config.toml init'
echo 'Configure the Samba share before starting tier and restarting smbd.'
