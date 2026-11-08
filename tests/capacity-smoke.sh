#!/usr/bin/env bash
set -euo pipefail
# Run as root in the project directory, passing the compiled capacity test as $1.
mountpoint=$(mktemp -d)
trap 'mountpoint -q "$mountpoint" && umount "$mountpoint"; rmdir "$mountpoint"' EXIT
mount -t tmpfs -o size=8M tmpfs "$mountpoint"
TIER_TEST_CACHE="$mountpoint" "$1" --ignored --exact full_cache_respects_the_configured_confirmation_policy
