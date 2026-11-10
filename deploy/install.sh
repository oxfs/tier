#!/bin/sh
set -eu
cd "$(dirname "$0")"
# shellcheck source=/dev/null
. /etc/os-release
if [ "$ID" != debian ] || [ "$VERSION_ID" != 13 ]; then
    echo 'This package requires Debian 13.' >&2
    exit 1
fi
[ "$(dpkg --print-architecture)" = "$(cat architecture)" ] || {
    echo 'Package architecture does not match this host.' >&2
    exit 1
}
[ "$(dpkg-query -W -f='${Version}' samba)" = "$(cat samba-version)" ] || {
    echo 'Samba package version does not match tier.so; install a matching Tier package.' >&2
    exit 1
}
for dependency in libprotobuf-c1 libjemalloc2; do
    [ "$(dpkg-query -W -f='${Status}' "$dependency" 2>/dev/null)" = 'install ok installed' ] || {
        echo "Tier requires $dependency; install it with apt install $dependency." >&2
        exit 1
    }
done
getent group tier >/dev/null || groupadd --system tier
module_dir=$(smbd -b | awk '$1 == "MODULESDIR:" {print $2}')
[ -n "$module_dir" ] || {
    echo 'Cannot locate Samba modules.' >&2
    exit 1
}
install -d /usr/local/bin "$module_dir/vfs"
staged=$(mktemp /usr/local/bin/.tier.XXXXXX)
module=$(mktemp "$module_dir/vfs/.tier.XXXXXX")
trap 'rm -f "$staged" "$module"' EXIT
install -m755 tier "$staged"
install -m755 tier.so "$module"
mv -f "$staged" /usr/local/bin/tier
mv -f "$module" "$module_dir/vfs/tier.so"
install -d -m755 /etc/tier
install -d -m700 /var/lib/tier
if [ ! -e /etc/tier/tier.toml ]; then
    install -m640 tier.toml /etc/tier/tier.toml
fi
install -Dm644 tier.service /etc/systemd/system/tier.service
if [ -d /run/systemd/system ]; then systemctl daemon-reload; fi
printf '%s\n' 'Review /etc/tier/tier.toml, merge samba-share.conf, then start tier and smbd.'
