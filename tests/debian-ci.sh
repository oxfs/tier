#!/usr/bin/env bash
# Debian 13 integration container; the Tier binary is built on Ubuntu.
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
export DEBIAN_FRONTEND=noninteractive
sed -i 's/Types: deb$/Types: deb deb-src/' /etc/apt/sources.list.d/debian.sources
apt-get update -qq
apt-get install -y --no-install-recommends build-essential pkg-config ca-certificates \
    python3 python3-dev python3-venv samba samba-dev smbclient protobuf-c-compiler libprotobuf-c-dev
apt-get build-dep -y samba
python3 -m venv /build/smb-python
/build/smb-python/bin/pip install --disable-pip-version-check smbprotocol==1.17.0
export PATH="/build/smb-python/bin:$PATH"
export TIER_SAMBA_BUILD_DIR=/build/samba-source
"$repo/samba/build.sh" /build/module
"$repo/deploy/package.sh" "${TIER_BIN:?provide Ubuntu Tier binary}" /build/module /build/artifacts
arch=$(cat /build/module/architecture)
tar -C /build -xzf "/build/artifacts/tier-debian13-$arch.tar.gz"
/build/tier-debian13-"$arch"/install.sh
TIER_BENCHMARK=1 TIER_BENCH_OUTPUT=/build/artifacts/benchmark.json TIER_BIN=/usr/local/bin/tier "$repo/tests/smb-smoke.sh"
