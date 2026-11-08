#!/bin/sh
set -eu
cd "$(dirname "$0")"
getent group tier >/dev/null || groupadd --system tier
install -d /usr/local/bin
staged=$(mktemp /usr/local/bin/.tier.XXXXXX)
trap 'rm -f "$staged"' EXIT
install -m755 tier "$staged"
mv -f "$staged" /usr/local/bin/tier
install -d -m755 /etc/tier
install -d -m700 /var/lib/tier
mkdir -p /srv/tier/share
if [ ! -e /etc/tier/tier.toml ]; then
    install -m640 tier.toml /etc/tier/tier.toml
fi
install -Dm644 tier.service /etc/systemd/system/tier.service
systemctl daemon-reload
printf '%s\n' 'Review /etc/tier/tier.toml, then run: systemctl enable --now tier'