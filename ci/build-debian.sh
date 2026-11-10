#!/usr/bin/env bash
# Debian 13 integration container; the Tier binary is built on Ubuntu.
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
if [[ ${1:-} == prepare ]]; then
    export DEBIAN_FRONTEND=noninteractive
    sed -i 's/Types: deb$/Types: deb deb-src/' /etc/apt/sources.list.d/debian.sources
    apt-get update -qq
    apt-get install -y --no-install-recommends build-essential pkg-config ca-certificates ccache \
        python3 python3-dev samba samba-dev smbclient protobuf-c-compiler libprotobuf-c-dev
    apt-get build-dep -y samba
    {
        dpkg-query -W
        dpkg --print-architecture
        cc --version
        dpkg-buildflags --export
    } | sha256sum | cut -d' ' -f1 >/build/environment-key
    exit 0
fi
export TIER_SAMBA_BUILD_DIR=/build/samba-source
export CCACHE_DIR=/build/ccache CCACHE_MAXSIZE=512M
"$repo/samba/build.sh" /build/module
ccache --show-stats
"$repo/deploy/package.sh" "${TIER_BIN:?provide Ubuntu Tier binary}" /build/module /build/artifacts
