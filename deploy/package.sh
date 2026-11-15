#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
binary=${1:-"$root/target/release/tier"}
module=${2:-"$root/target/samba/tier.so"}
output=${3:-"$root/target/packages"}
arch=$(dpkg --print-architecture)
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir -p "$output" "$stage/tier/bin" "$stage/tier/lib" "$stage/tier/deploy"
install -m 0755 "$binary" "$stage/tier/bin/tier"
install -m 0755 "$module" "$stage/tier/lib/tier.so"
cp -r "$root/web/dist" "$stage/tier/web"
install -m 0755 "$root/deploy/install.sh" "$stage/tier/deploy/"
install -m 0644 "$root/deploy/"{tier.service,tier-hardware.service,tier-hardware.timer,config.toml.example,smb.conf.example} "$stage/tier/deploy/"
install -m 0644 "$root/deploy/"{Dockerfile,container.sh,compose.yml,setup.sh,setup.py} "$stage/tier/deploy/"
"$binary" --version | cut -d ' ' -f2 >"$stage/tier/deploy/version"
# Preserve Docker build arguments literally.
# shellcheck disable=SC2016
sed -i 's|ARG PACKAGE_DIR=target/docker-package|ARG PACKAGE_DIR=.|; s|${PACKAGE_DIR}/tier/|${PACKAGE_DIR}/|g' "$stage/tier/deploy/Dockerfile"
dpkg-query -W -f='${Version}\n' samba >"$stage/tier/deploy/samba-version"
tar -C "$stage" -czf "$output/tier-debian13-$arch.tar.gz" tier
(cd "$output" && sha256sum "tier-debian13-$arch.tar.gz" >SHA256SUMS)
cat "$output/SHA256SUMS"
