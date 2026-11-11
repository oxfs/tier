#!/usr/bin/env bash
# Build against the installed Debian Samba source, including Debian's config.
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
work=${TIER_SAMBA_BUILD_DIR:-/tmp/tier-samba-build}
out=${1:-"$work/output"}
client=${TIER_CLIENT_LIB:-"$repo/target/release/libtier_client.a"}
[[ -f "$client" ]] || {
    echo "Missing $client; run cargo build --release --workspace first." >&2
    exit 1
}
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
cp "$repo/samba/client/tier_rust.h" "$source_dir/source3/modules/"
cp "$client" "$source_dir/source3/modules/libtier_client.a"
python3 - "$source_dir" <<'PYBUILD'
from pathlib import Path
import re
import sys
root = Path(sys.argv[1])
# Existing build trees may still contain the former protobuf-c module definition.
p = root / 'source3/wscript'
s = p.read_text()
check = "    conf.CHECK_LIB('protobuf-c', shlib=True, mandatory=True)\n"
if check in s:
    p.write_text(s.replace(check, ''))
    (root / 'bin/configured.stamp').unlink(missing_ok=True)
p = root / 'source3/modules/wscript_build'
s = re.sub(r"\nbld\.SAMBA3_MODULE\('vfs_tier'.*?enabled=True\)\n", "\n", p.read_text(), flags=re.S)
marker = '\n# Tier Rust client linkage\n'
s = s.split(marker)[0]
p.write_text(s + marker + """
from waflib import Utils
from waflib.TaskGen import feature, after_method

@feature('tier_rust_client')
@after_method('apply_link')
def link_tier_rust_client(self):
    archive = self.path.find_resource('libtier_client.a')
    self.link_task.dep_nodes.append(archive)
    self.link_task.env = self.link_task.env.derive()
    self.link_task.env.append_value('STLIBPATH', archive.parent.abspath())
    self.link_task.env.append_value('STLIB', 'tier_client')
    self.link_task.env.append_value('LIB', ['pthread', 'dl', 'm'])
    self.link_task.env.append_value('LINKFLAGS', '-Wl,--exclude-libs,ALL')

bld.SAMBA3_MODULE('vfs_tier', subsystem='vfs',
    source='vfs_tier.c tier_client.c',
    deps='samba-util', cflags='-fvisibility=hidden',
    init_function='', internal_module=False, enabled=True)
module = bld.get_tgen_by_name('vfs_tier')
module.features = Utils.to_list(module.features) + ['tier_rust_client']
""")
PYBUILD
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
