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
cd "$source_dir"
make -f debian/rules bin/configured.stamp
# samba-dev omits private VFS headers. Use Samba's own generators without
# invoking Waf's build groups, which also build unrelated compiler helpers.
"$repo/samba/generate-headers.sh" "$source_dir"

protoc-c -I "$repo/proto" --c_out="$out" "$repo/proto/tier.proto"
read -r -a cflags <<<"$(dpkg-buildflags --get CFLAGS)"
read -r -a cppflags <<<"$(dpkg-buildflags --get CPPFLAGS)"
read -r -a ldflags <<<"$(dpkg-buildflags --get LDFLAGS)"
libdir="/usr/lib/$(dpkg-architecture -qDEB_HOST_MULTIARCH)"
includes=(-I"$out" -I"$source_dir/bin/default/include" -I"$source_dir/source3"
    -I"$source_dir/source3/include" -I"$source_dir/lib/replace" -I"$source_dir"
    -I"$source_dir/bin/default" -I"$source_dir/bin/default/source3")
for unit in vfs_tier tier_client; do
    cc "${cflags[@]}" "${cppflags[@]}" "${includes[@]}" -fPIC \
        -DHAVE_CONFIG_H=1 -D_SAMBA_BUILD_=4 -D_GNU_SOURCE=1 -D__STDC_WANT_LIB_EXT1__=1 \
        -Werror=implicit-function-declaration -c "$repo/samba/$unit.c" -o "$out/$unit.o"
done
cc "${cflags[@]}" "${cppflags[@]}" -fPIC -c "$out/tier.pb-c.c" -o "$out/tier.pb-c.o"
# All libraries are from the installed matching Debian packages. Undefined
# symbols fail the build; there is no runtime path into this source tree.
cc -shared "${ldflags[@]}" -Wl,-z,now -Wl,--as-needed -Wl,--no-undefined \
    -o "$out/tier.so" "$out/vfs_tier.o" "$out/tier_client.o" "$out/tier.pb-c.o" \
    -lsamba-util -ltevent -ltalloc -lsmbconf -ltevent-util -lprotobuf-c \
    -L"$libdir/samba" -l:libsmbd-base-private-samba.so.0 \
    -l:libreplace-private-samba.so.0 -l:libsamba-debug-private-samba.so.0 \
    -l:libsmbd-shim-private-samba.so.0
printf '%s\n' "$version" >"$out/samba-version"
printf '%s\n' "$(dpkg --print-architecture)" >"$out/architecture"
