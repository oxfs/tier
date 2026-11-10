#!/usr/bin/env bash
# Validate the deliverable without Samba build trees or development packages.
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y --no-install-recommends samba smbclient libprotobuf-c1 ca-certificates python3-venv
python3 -m venv /tmp/smb-python
/tmp/smb-python/bin/pip install --disable-pip-version-check -r "$repo/tests/smb/requirements.txt"
export PATH="/tmp/smb-python/bin:$PATH"
mkdir /tmp/tier-package
tar -C /tmp/tier-package -xzf "${TIER_PACKAGE:?provide the Debian package}"
/tmp/tier-package/tier-debian13-"$(dpkg --print-architecture)"/install.sh
SMB_TEST_LOG_LEVEL=5 TIER_TEST_LOG_DIR=/artifacts/logs TIER_BENCHMARK=1 TIER_BENCH_OUTPUT=/artifacts/benchmark.json \
    TIER_BIN=/usr/local/bin/tier "$repo/tests/smb/run.sh"
