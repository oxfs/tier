#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
work=${TIER_SAMBA_BUILD_DIR:-/tmp/tier-samba}
out=${1:-"$root/target/samba"}
mkdir -p "$work" "$out"
out=$(cd "$out" && pwd)
version=$(dpkg-query -W -f='${source:Version}' samba)
source="$work/source"
if [[ ! -d "$source" ]]; then
    (cd "$work" && apt-get source --download-only "samba=$version" && dpkg-source -x ./*.dsc source)
fi
[[ $(dpkg-parsechangelog -l "$source/debian/changelog" -S Version) == "$version" ]]
if [[ -z ${TIER_LIB:-} ]]; then
    cargo build --manifest-path "$root/Cargo.toml" --release --locked
    TIER_LIB="$root/target/release/libtier.a"
fi
mkdir -p "$source/source3/modules/tier"
cp "$root/samba/"{vfs_tier.c,wscript_build} "$source/source3/modules/tier/"
mkdir -p "$source/bin"
cp "$TIER_LIB" "$source/bin/libtier.a"
if ! grep -Fq "bld.RECURSE('tier')" "$source/source3/modules/wscript_build"; then
    printf '\nbld.RECURSE('\''tier'\'')\n' >>"$source/source3/modules/wscript_build"
fi
cd "$source"
make -f debian/rules bin/configured.stamp
PYTHONHASHSEED=1 ./buildtools/bin/waf build --targets=vfs_tier -j "${JOBS:-2}"
install -m755 bin/modules/vfs/tier.so "$out/tier.so"
