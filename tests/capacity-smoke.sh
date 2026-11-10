#!/usr/bin/env bash
set -euo pipefail
cache=$(mktemp -d)
privilege=()
if ((EUID != 0)); then privilege=(sudo); fi
cleanup() {
    if mountpoint -q "$cache"; then "${privilege[@]}" umount "$cache"; fi
    rmdir "$cache"
}
trap cleanup EXIT
"${privilege[@]}" mount -t tmpfs -o "size=8M,uid=$(id -u),gid=$(id -g)" tmpfs "$cache"
TIER_TEST_CACHE="$cache" cargo test --locked --test capacity -- --ignored --nocapture
