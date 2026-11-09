#!/usr/bin/env bash
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
binary=$(realpath "$1")
module=$(realpath "$2")
output=$(realpath -m "$3")
arch=$(cat "$module/architecture")
name="tier-debian13-$arch"
mkdir -p "$output"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir "$stage/$name"
install -m755 "$binary" "$stage/$name/tier"
install -m755 "$module/tier.so" "$stage/$name/tier.so"
cp "$module/"{architecture,samba-version} "$stage/$name/"
cp "$repo/deploy/"{install.sh,tier.toml,tier.service,samba-share.conf} "$stage/$name/"
cp "$repo/README.md" "$stage/$name/"
tar -C "$stage" -czf "$output/$name.tar.gz" "$name"
(cd "$output" && sha256sum "$name.tar.gz" > SHA256SUMS)
