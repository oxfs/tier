#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
work=${TIER_SAMBA_BUILD_DIR:-/tmp/tier-v2-samba}
out=${1:-"$root/target/samba"}
mkdir -p "$work" "$out"
out=$(cd "$out" && pwd)
version=$(dpkg-query -W -f='${source:Version}' samba)
source="$work/source"
if [[ ! -d "$source" ]]; then
    (cd "$work" && apt-get source --download-only "samba=$version" && dpkg-source -x ./*.dsc source)
fi
[[ $(dpkg-parsechangelog -l "$source/debian/changelog" -S Version) == "$version" ]]
if [[ -z ${TIER_V2_LIB:-} ]]; then
    cargo build --manifest-path "$root/Cargo.toml" --release --locked
    TIER_V2_LIB="$root/target/release/libtier_v2.a"
fi
mkdir -p "$source/source3/modules/tier_v2"
cp "$root/samba/"{vfs_tier_v2.c,wscript_build} "$source/source3/modules/tier_v2/"
mkdir -p "$source/bin"
cp "$TIER_V2_LIB" "$source/bin/libtier_v2.a"
if ! grep -Fq "bld.RECURSE('tier_v2')" "$source/source3/modules/wscript_build"; then
    printf '\nbld.RECURSE('\''tier_v2'\'')\n' >>"$source/source3/modules/wscript_build"
fi
cd "$source"
make -f debian/rules bin/configured.stamp
PYTHONHASHSEED=1 ./buildtools/bin/waf build --targets=vfs_tier_v2 -j "${JOBS:-2}"
install -m755 bin/modules/vfs/tier_v2.so "$out/tier_v2.so"
