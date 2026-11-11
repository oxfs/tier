#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
binary=${1:-"$root/target/release/tier-v2"}
module=${2:-"$root/target/samba/tier_v2.so"}
output=${3:-"$root/target/packages"}
arch=$(dpkg --print-architecture)
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir -p "$output" "$stage/tier-v2/bin" "$stage/tier-v2/lib" "$stage/tier-v2/deploy"
install -m 0755 "$binary" "$stage/tier-v2/bin/tier-v2"
install -m 0755 "$module" "$stage/tier-v2/lib/tier_v2.so"
install -m 0755 "$root/deploy/install.sh" "$stage/tier-v2/deploy/"
install -m 0644 "$root/deploy/"{tier-v2.service,config.toml.example,smb.conf.example} "$stage/tier-v2/deploy/"
dpkg-query -W -f='${Version}\n' samba >"$stage/tier-v2/deploy/samba-version"
tar -C "$stage" -czf "$output/tier-v2-debian13-$arch.tar.gz" tier-v2
sha256sum "$output/tier-v2-debian13-$arch.tar.gz"
