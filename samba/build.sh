#!/usr/bin/env bash
# Build against the installed Debian Samba source, including Debian's config.
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
work=${TIER_SAMBA_BUILD_DIR:-/tmp/tier-samba-build}
out=${1:-"$work/output"}
mkdir -p "$work" "$out"
out=$(cd "$out" && pwd)
version=$(dpkg-query -W -f='${Version}' samba)
source_version=$(dpkg-query -W -f='${source:Version}' samba)
source_name=$(dpkg-query -W -f='${source:Package}' samba)
# Each source package gets its own build tree. Sorting old source directories
# could silently pick a different ABI after a downgrade or security update.
source_key=${source_version//:/_}
mkdir -p "$work/$source_key"
cd "$work/$source_key"
source_dir="$PWD/source"
if [[ ! -d "$source_dir" ]]; then
    apt-get source --download-only "$source_name=$source_version"
    descriptors=("$source_name"_*.dsc)
    [[ ${#descriptors[@]} == 1 && -f ${descriptors[0]} ]]
    dpkg-source -x "${descriptors[0]}" "$source_dir"
fi
[[ $(dpkg-parsechangelog -l "$source_dir/debian/changelog" -S Version) == "$source_version" ]]
cp "$repo/samba/"{vfs_tier.c,tier_client.c,tier_client.h} "$source_dir/source3/modules/"
protoc-c -I "$repo/proto" --c_out="$source_dir/source3/modules" "$repo/proto/tier.proto"
python3 - "$source_dir" <<'PY'
from pathlib import Path
import sys
root = Path(sys.argv[1])
p = root / 'source3/wscript'
s = p.read_text()
check = "    conf.CHECK_LIB('protobuf-c', shlib=True, mandatory=True)"
if check not in s:
    p.write_text(s.replace('def configure(conf):', 'def configure(conf):\n' + check, 1))
    (root / 'bin/configured.stamp').unlink(missing_ok=True)
p = root / 'source3/modules/wscript_build'
s = p.read_text()
if "bld.SAMBA3_MODULE('vfs_tier'" not in s:
    p.write_text(s + """
bld.SAMBA3_MODULE('vfs_tier', subsystem='vfs',
    source='vfs_tier.c tier_client.c tier.pb-c.c',
    deps='samba-util protobuf-c', cflags='-fvisibility=hidden',
    init_function='', internal_module=False, enabled=True)
""")
PY
cd "$source_dir"
if command -v ccache >/dev/null; then
    export PATH="/usr/lib/ccache:$PATH"
    export CCACHE_BASEDIR="$work"
fi
make -f debian/rules bin/configured.stamp
PYTHONHASHSEED=1 ./buildtools/bin/waf build --targets=vfs_tier -j "${JOBS:-2}"
module=bin/modules/vfs/tier.so
[[ $(nm -D --defined-only --format=posix "$module" | awk '$2 != "A" {sub(/@.*/, "", $1); print $1}') == samba_init_module ]]
if readelf -d "$module" | grep -E '(RPATH|RUNPATH)' | grep -F "$work"; then
    echo 'Module contains a build-directory runtime path' >&2
    exit 1
fi
install -m755 "$module" "$out/tier.so"
printf '%s\n' "$version" >"$out/samba-version"
printf '%s\n' "$(dpkg --print-architecture)" >"$out/architecture"
