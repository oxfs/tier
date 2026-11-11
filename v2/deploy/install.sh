#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
binary=${1:-"$root/bin/tier-v2"}
module=${2:-"$root/lib/tier_v2.so"}
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
if ! id tier-v2 >/dev/null 2>&1; then
    useradd --system --user-group --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin tier-v2
fi
install -m 0755 "$binary" /usr/local/bin/tier-v2
install -m 0755 "$module" "$modules/vfs/tier_v2.so"
install -m 0644 "$root/deploy/tier-v2.service" /etc/systemd/system/tier-v2.service
install -d -m 0750 -o root -g tier-v2 /etc/tier-v2
if [[ -d /run/systemd/system ]]; then
    systemctl daemon-reload
fi
echo 'Installed. Configure /etc/tier-v2/config.toml (readable by tier-v2), make storage roots writable by tier-v2, then run:'
echo '  runuser -u tier-v2 -- tier-v2 /etc/tier-v2/config.toml init'
echo 'Configure the Samba share before starting tier-v2 and restarting smbd.'
