#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
if [[ ${1:-} == prepare ]]; then
    export DEBIAN_FRONTEND=noninteractive
    sed -i 's/Types: deb$/Types: deb deb-src/' /etc/apt/sources.list.d/debian.sources
    apt-get update -qq
    apt-get install -y --no-install-recommends build-essential pkg-config ca-certificates python3 python3-samba samba samba-dev smbclient
    apt-get build-dep -y samba
    dpkg-query -W | sha256sum | cut -d' ' -f1 >/build/environment-key
    exit 0
fi

export TIER_SAMBA_BUILD_DIR=/build/samba
export TIER_LIB="$root/target/release/libtier.a"
export TIER_BIN="$root/target/release/tier"
"$root/samba/build.sh" /build/module
"$root/deploy/package.sh" "$TIER_BIN" /build/module/tier.so /build/artifacts
mkdir -p /build/runtime
tar -xzf "/build/artifacts/tier-debian13-$(dpkg --print-architecture).tar.gz" -C /build/runtime
bash /build/runtime/tier/deploy/install.sh
TIER_USER=tier "$root/tests/smb.sh"
python3 "$root/tests/archive-smb.py"
